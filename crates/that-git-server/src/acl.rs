/// A parsed ref update command from git-receive-pack request body.
#[derive(Debug, Clone)]
pub struct RefCommand {
    pub old: String,
    pub new: String,
    pub refname: String,
}

/// Parse pkt-line ref commands from the receive-pack request body.
/// Format per line: `<old-sha> <new-sha> <refname>\0<capabilities>` (first line has caps).
pub fn parse_ref_commands(body: &[u8]) -> Vec<RefCommand> {
    let mut refs = Vec::new();
    let mut pos = 0;

    while pos + 4 <= body.len() {
        let hex = &body[pos..pos + 4];
        let len_str = std::str::from_utf8(hex).unwrap_or("0000");
        let pkt_len = usize::from_str_radix(len_str, 16).unwrap_or(0);

        if pkt_len == 0 {
            break; // flush packet
        }
        if pkt_len < 4 || pos + pkt_len > body.len() {
            break;
        }

        let data = &body[pos + 4..pos + pkt_len];
        // Strip capabilities after NUL byte
        let line = if let Some(nul) = data.iter().position(|&b| b == 0) {
            &data[..nul]
        } else {
            data
        };

        if let Ok(s) = std::str::from_utf8(line) {
            let s = s.trim();
            let parts: Vec<&str> = s.splitn(3, ' ').collect();
            if parts.len() == 3 {
                refs.push(RefCommand {
                    old: parts[0].to_string(),
                    new: parts[1].to_string(),
                    refname: parts[2].to_string(),
                });
            }
        }

        pos += pkt_len;
    }

    refs
}

/// Check that an agent is only pushing to its own task branch.
/// Returns Err with a message if the push violates the ACL.
pub fn check(refs: &[RefCommand], agent_name: &str) -> Result<(), String> {
    let allowed_prefix = format!("refs/heads/task/{agent_name}");
    for r in refs {
        if !r.refname.starts_with(&allowed_prefix) {
            return Err(format!(
                "agent '{}' cannot push to '{}' — only '{}' allowed",
                agent_name, r.refname, allowed_prefix
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_pkt_line(data: &str) -> Vec<u8> {
        let len = data.len() + 4;
        let mut out = format!("{len:04x}").into_bytes();
        out.extend(data.as_bytes());
        out
    }

    #[test]
    fn parse_single_ref() {
        let mut body = make_pkt_line(
            "0000000000000000000000000000000000000000 abc123def456 refs/heads/task/worker-1\0 report-status",
        );
        body.extend(b"0000");

        let refs = parse_ref_commands(&body);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].refname, "refs/heads/task/worker-1");
    }

    #[test]
    fn acl_allows_own_branch() {
        let refs = vec![RefCommand {
            old: "0".repeat(40),
            new: "a".repeat(40),
            refname: "refs/heads/task/worker-1".into(),
        }];
        assert!(check(&refs, "worker-1").is_ok());
    }

    #[test]
    fn acl_denies_other_branch() {
        let refs = vec![RefCommand {
            old: "0".repeat(40),
            new: "a".repeat(40),
            refname: "refs/heads/main".into(),
        }];
        assert!(check(&refs, "worker-1").is_err());
    }

    #[test]
    fn acl_denies_other_agents_branch() {
        let refs = vec![RefCommand {
            old: "0".repeat(40),
            new: "a".repeat(40),
            refname: "refs/heads/task/worker-2".into(),
        }];
        assert!(check(&refs, "worker-1").is_err());
    }

    #[test]
    fn parse_multiple_refs() {
        let mut body = make_pkt_line(
            "0000000000000000000000000000000000000000 aaaa refs/heads/task/w1\0 caps",
        );
        body.extend(make_pkt_line(
            "1111111111111111111111111111111111111111 bbbb refs/heads/task/w1-sub",
        ));
        body.extend(b"0000");

        let refs = parse_ref_commands(&body);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].refname, "refs/heads/task/w1");
        assert_eq!(refs[1].refname, "refs/heads/task/w1-sub");
    }

    #[test]
    fn parse_no_capabilities() {
        let mut body =
            make_pkt_line("0000000000000000000000000000000000000000 ffff refs/heads/main");
        body.extend(b"0000");

        let refs = parse_ref_commands(&body);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].refname, "refs/heads/main");
        assert_eq!(refs[0].new, "ffff");
    }

    #[test]
    fn parse_empty_body() {
        assert!(parse_ref_commands(b"").is_empty());
        assert!(parse_ref_commands(b"0000").is_empty());
    }

    #[test]
    fn parse_malformed_body() {
        // Too short to be a valid pkt-line
        assert!(parse_ref_commands(b"00").is_empty());
        // Valid pkt-line but only 2 parts (not enough for old/new/ref)
        let body = make_pkt_line("only two-parts");
        assert!(parse_ref_commands(&body).is_empty());
    }

    #[test]
    fn acl_allows_subbranch() {
        // task/worker-1/feature should be allowed for worker-1
        let refs = vec![RefCommand {
            old: "0".repeat(40),
            new: "a".repeat(40),
            refname: "refs/heads/task/worker-1/feature-x".into(),
        }];
        assert!(check(&refs, "worker-1").is_ok());
    }

    #[test]
    fn acl_empty_refs_passes() {
        assert!(check(&[], "worker-1").is_ok());
    }

    #[test]
    fn acl_mixed_refs_fails_on_first_violation() {
        let refs = vec![
            RefCommand {
                old: "0".repeat(40),
                new: "a".repeat(40),
                refname: "refs/heads/task/worker-1".into(),
            },
            RefCommand {
                old: "0".repeat(40),
                new: "b".repeat(40),
                refname: "refs/heads/main".into(),
            },
        ];
        let err = check(&refs, "worker-1").unwrap_err();
        assert!(err.contains("main"));
    }
}
