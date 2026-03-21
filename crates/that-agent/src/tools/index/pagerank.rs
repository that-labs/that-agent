//! PageRank-based file importance scoring.
//!
//! Computes file importance by treating cross-file references as a directed
//! graph and running PageRank. Files that are referenced by many other files
//! (especially important ones) score higher.

use crate::tools::index::SymbolIndex;
use petgraph::graph::DiGraph;
use petgraph::visit::EdgeRef;
use std::collections::HashMap;

/// Compute PageRank scores for all indexed files.
///
/// 1. Query all file-to-file edges from the refs table
/// 2. Build a directed graph with files as nodes and reference counts as edges
/// 3. Run PageRank with damping factor 0.85 and 30 iterations
/// 4. Store results in the file_scores table
/// 5. Return path -> score mapping
pub fn compute_pagerank(
    index: &SymbolIndex,
) -> Result<HashMap<String, f64>, crate::tools::index::IndexError> {
    let edges = index.file_edges()?;

    // Collect unique file paths
    let mut file_set: Vec<String> = Vec::new();
    let mut file_to_idx: HashMap<String, usize> = HashMap::new();

    for (src, dst, _) in &edges {
        if !file_to_idx.contains_key(src) {
            file_to_idx.insert(src.clone(), file_set.len());
            file_set.push(src.clone());
        }
        if !file_to_idx.contains_key(dst) {
            file_to_idx.insert(dst.clone(), file_set.len());
            file_set.push(dst.clone());
        }
    }

    if file_set.is_empty() {
        // No edges → assign uniform scores to all indexed files and store them
        let all_files = index.all_files()?;
        if all_files.is_empty() {
            return Ok(HashMap::new());
        }
        let uniform = 1.0 / all_files.len() as f64;
        let result: HashMap<String, f64> = all_files
            .into_iter()
            .map(|(_, path)| (path, uniform))
            .collect();
        index.store_pagerank_scores(&result)?;
        return Ok(result);
    }

    // Build petgraph DiGraph
    let mut graph = DiGraph::<&str, f64>::new();
    let mut node_indices = Vec::new();

    for path in &file_set {
        node_indices.push(graph.add_node(path.as_str()));
    }

    for (src, dst, count) in &edges {
        let src_idx = node_indices[file_to_idx[src]];
        let dst_idx = node_indices[file_to_idx[dst]];
        graph.add_edge(src_idx, dst_idx, *count as f64);
    }

    // Run PageRank
    let scores = pagerank(&graph, 0.85, 30);

    // Map back to file paths
    let mut result: HashMap<String, f64> = HashMap::new();
    for (i, score) in scores.iter().enumerate() {
        result.insert(file_set[i].clone(), *score);
    }

    // Store in index
    index.store_pagerank_scores(&result)?;

    Ok(result)
}

/// Simple PageRank implementation.
///
/// Iteratively computes the importance of each node in a directed graph.
/// - `damping`: probability of following an edge (typically 0.85)
/// - `iterations`: number of power iterations
fn pagerank(graph: &DiGraph<&str, f64>, damping: f64, iterations: usize) -> Vec<f64> {
    let n = graph.node_count();
    if n == 0 {
        return vec![];
    }

    let initial = 1.0 / n as f64;
    let mut scores = vec![initial; n];
    let mut new_scores = vec![0.0; n];

    for _ in 0..iterations {
        let base = (1.0 - damping) / n as f64;
        new_scores.fill(base);

        for node_idx in graph.node_indices() {
            let out_degree: f64 = graph.edges(node_idx).map(|e| *e.weight()).sum();

            if out_degree > 0.0 {
                let share = scores[node_idx.index()] * damping / out_degree;
                for edge in graph.edges(node_idx) {
                    let target = edge.target().index();
                    let weight = *edge.weight();
                    new_scores[target] += share * weight;
                }
            } else {
                // Dangling node: distribute equally
                let share = scores[node_idx.index()] * damping / n as f64;
                for score in new_scores.iter_mut() {
                    *score += share;
                }
            }
        }

        std::mem::swap(&mut scores, &mut new_scores);
    }

    scores
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pagerank_empty_graph() {
        let graph = DiGraph::<&str, f64>::new();
        let scores = pagerank(&graph, 0.85, 30);
        assert!(scores.is_empty());
    }

    #[test]
    fn test_pagerank_single_node() {
        let mut graph = DiGraph::new();
        graph.add_node("a");
        let scores = pagerank(&graph, 0.85, 30);
        assert_eq!(scores.len(), 1);
        assert!(
            (scores[0] - 1.0).abs() < 0.01,
            "single node should have score ~1.0"
        );
    }

    #[test]
    fn test_pagerank_two_nodes_one_edge() {
        let mut graph = DiGraph::new();
        let a = graph.add_node("a");
        let b = graph.add_node("b");
        graph.add_edge(a, b, 1.0);

        let scores = pagerank(&graph, 0.85, 30);
        assert_eq!(scores.len(), 2);
        // b should have higher score since a points to it
        assert!(
            scores[b.index()] > scores[a.index()],
            "target node should score higher: a={}, b={}",
            scores[a.index()],
            scores[b.index()]
        );
    }

    #[test]
    fn test_pagerank_star_topology() {
        // Many nodes pointing to one central node
        let mut graph = DiGraph::new();
        let center = graph.add_node("center");
        for i in 0..5 {
            let node = graph.add_node(Box::leak(format!("spoke_{}", i).into_boxed_str()));
            graph.add_edge(node, center, 1.0);
        }

        let scores = pagerank(&graph, 0.85, 30);
        let center_score = scores[center.index()];
        let spoke_score = scores[1]; // Any spoke

        assert!(
            center_score > spoke_score,
            "center should score highest: center={}, spoke={}",
            center_score,
            spoke_score
        );
    }

    #[test]
    fn test_pagerank_scores_sum_to_one() {
        let mut graph = DiGraph::new();
        let a = graph.add_node("a");
        let b = graph.add_node("b");
        let c = graph.add_node("c");
        graph.add_edge(a, b, 1.0);
        graph.add_edge(b, c, 1.0);
        graph.add_edge(c, a, 1.0);

        let scores = pagerank(&graph, 0.85, 30);
        let sum: f64 = scores.iter().sum();
        assert!(
            (sum - 1.0).abs() < 0.01,
            "scores should sum to ~1.0, got {}",
            sum
        );
    }

    #[test]
    fn test_compute_pagerank_with_index() {
        use crate::tools::index::SymbolIndex;
        use std::fs;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(
            tmp.path().join("src").join("main.rs"),
            "fn main() {\n    let config = Config::new();\n}\n",
        )
        .unwrap();
        fs::write(
            tmp.path().join("src").join("config.rs"),
            "pub struct Config {}\nimpl Config {\n    pub fn new() -> Self { Config {} }\n}\n",
        )
        .unwrap();

        let index = SymbolIndex::open_in_memory().unwrap();
        index.build(tmp.path()).unwrap();

        let scores = compute_pagerank(&index).unwrap();
        // Computation should succeed without errors (scores may be empty
        // if no cross-file refs are resolved)
        let _ = scores;
    }
}
