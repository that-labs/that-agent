//! OAuth edge-case tester matching zeroclaw's exact approach.
//!   cargo run -p that-core --example oauth_test

use serde::Serialize;

#[derive(Serialize)]
struct Request {
    model: String,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    messages: Vec<Msg>,
    temperature: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<Tool>>,
}

#[derive(Serialize)]
struct Msg {
    role: String,
    content: String,
}

#[derive(Serialize)]
struct Tool {
    name: String,
    description: String,
    input_schema: serde_json::Value,
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    let token = std::env::var("CLAUDE_CODE_OAUTH_TOKEN").expect("CLAUDE_CODE_OAUTH_TOKEN not set");
    println!("Token prefix: {}...", &token[..25]);

    let client = reqwest::Client::new();

    // Test 1: zeroclaw-exact minimal (no system, no tools, no stream)
    let req = Request {
        model: "claude-opus-4-6".into(),
        max_tokens: 4096,
        system: None,
        messages: vec![Msg {
            role: "user".into(),
            content: "hi".into(),
        }],
        temperature: 0.7,
        tools: None,
    };
    let resp = client
        .post("https://api.anthropic.com/v1/messages")
        .header("anthropic-version", "2023-06-01")
        .header("Authorization", format!("Bearer {token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .json(&req)
        .send()
        .await
        .unwrap();
    println!("zeroclaw-exact (no sys, no tools): {}", resp.status());
    let _ = resp.text().await;

    // Test 2: with system string
    let req = Request {
        model: "claude-opus-4-6".into(),
        max_tokens: 4096,
        system: Some("You are helpful.".into()),
        messages: vec![Msg {
            role: "user".into(),
            content: "hi".into(),
        }],
        temperature: 0.7,
        tools: None,
    };
    let resp = client
        .post("https://api.anthropic.com/v1/messages")
        .header("anthropic-version", "2023-06-01")
        .header("Authorization", format!("Bearer {token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .json(&req)
        .send()
        .await
        .unwrap();
    println!("with system string: {}", resp.status());
    let _ = resp.text().await;

    // Test 3: our build_request style (json! macro + .body())
    let body = serde_json::json!({
        "model": "claude-opus-4-6",
        "max_tokens": 4096,
        "system": "You are helpful.",
        "messages": [{"role": "user", "content": "hi"}],
    });
    let resp = client
        .post("https://api.anthropic.com/v1/messages")
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .header("Authorization", format!("Bearer {token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    println!("json!() + .body(): {}", resp.status());
    let _ = resp.text().await;

    // Test 4: stream:true added
    let body = serde_json::json!({
        "model": "claude-opus-4-6",
        "max_tokens": 4096,
        "stream": true,
        "system": "You are helpful.",
        "messages": [{"role": "user", "content": "hi"}],
    });
    let resp = client
        .post("https://api.anthropic.com/v1/messages")
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .header("Authorization", format!("Bearer {token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    println!("stream:true + .body(): {}", resp.status());
    let _ = resp.text().await;

    // Test 5: with API key instead (control test)
    if let Ok(api_key) = std::env::var("ANTHROPIC_API_KEY") {
        let resp = client
            .post("https://api.anthropic.com/v1/messages")
            .header("anthropic-version", "2023-06-01")
            .header("x-api-key", &api_key)
            .json(&Request {
                model: "claude-opus-4-6".into(),
                max_tokens: 4096,
                system: Some("You are helpful.".into()),
                messages: vec![Msg {
                    role: "user".into(),
                    content: "hi".into(),
                }],
                temperature: 0.7,
                tools: None,
            })
            .send()
            .await
            .unwrap();
        println!("API key control: {}", resp.status());
        let _ = resp.text().await;
    }
}
