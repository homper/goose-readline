// Phase 2 exit criteria: connect to the in-process goose ACP agent,
// initialize, create a session, send a prompt, and assert the agent's reply
// arrives through our Client-side event channel.
//
// Requires network access and a configured provider (Z.AI Coding Plan key
// derived from kilo's auth.json, like scripts/run-goose.sh). Skips itself
// when no key is available.

use goose_readline::agent::{self, AgentEvent, Transport};
use goose_readline::agent::env_setup;

#[tokio::test]
async fn in_process_prompt_round_trip() {
    let has_key = std::env::var("ZAI_CODING_PLAN_API_KEY").is_ok()
        || env_setup::read_zai_key_from_kilo_auth().is_some();
    if !has_key {
        eprintln!("skipping: no Z.AI Coding Plan key configured");
        return;
    }
    env_setup::apply_defaults();

    let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
    let (result_tx, result_rx) = tokio::sync::oneshot::channel::<anyhow::Result<String>>();

    let handle = tokio::spawn(async move {
        let result_tx = std::sync::Arc::new(result_tx);
        let result_tx2 = result_tx.clone();
        drop(result_tx);
        let result = agent::connect_and_run(
            Transport::InProcess { builtins: vec!["developer".into()] },
            events_tx,
            move |cx| async move {
                let result_tx = result_tx2;
                let r: anyhow::Result<String> = async {
                // 1. initialize
                let caps = agent::initialize(&cx).await?;
                assert!(
                    caps.get("agentCapabilities").is_some(),
                    "initialize response missing agentCapabilities: {caps}"
                );

                // 2. new session
                let created = agent::new_session(&cx, &std::env::current_dir()?).await?;
                let sid = created["sessionId"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("session/new missing sessionId"))?
                    .to_string();

                // 3. prompt, driving the response from a spawned task so
                //    updates can be collected concurrently.
                let sent = agent::send_prompt(&cx, &sid, "Reply with exactly: OK");
                let (stop_tx, stop_rx) = tokio::sync::mpsc::channel::<()>(1);
                cx.spawn(async move {
                    let _ = sent.block_task().await;
                    let _ = stop_tx.send(()).await;
                    Ok(())
                })
                .map_err(|e| anyhow::anyhow!("spawn: {e}"))?;

                let mut agent_text = String::new();
                let mut got_text = false;
                let mut stop_rx = stop_rx;
                loop {
                    tokio::select! {
                        _ = stop_rx.recv() => break,
                        ev = events_rx.recv() => match ev {
                            Some(AgentEvent::Update { session_id, update }) => {
                                if session_id != sid { continue; }
                                use agent_client_protocol::schema::v1::{ContentBlock, SessionUpdate};
                                match update {
                                    SessionUpdate::AgentMessageChunk(chunk) => {
                                        if let ContentBlock::Text(t) = chunk.content {
                                            got_text = true;
                                            agent_text.push_str(&t.text);
                                        }
                                    }
                                    SessionUpdate::UsageUpdate(_)
                                    | SessionUpdate::ToolCall(_)
                                    | SessionUpdate::ToolCallUpdate(_) => {}
                                    _ => {}
                                }
                            }
                            Some(AgentEvent::ReadTextFile { request, responder }) => {
                                let _ = request;
                                let _ = responder.respond(
                                    agent_client_protocol::schema::v1::ReadTextFileResponse::new(""),
                                );
                            }
                            Some(AgentEvent::WriteTextFile { request, responder }) => {
                                let _ = request;
                                let _ = responder.respond(
                                    agent_client_protocol::schema::v1::WriteTextFileResponse::new(),
                                );
                            }
                            Some(AgentEvent::Permission { request, responder }) => {
                                // No tool use is expected for this prompt.
                                let _ = request;
                                let _ = responder.respond(
                                    agent_client_protocol::schema::v1::RequestPermissionResponse::new(
                                        agent_client_protocol::schema::v1::RequestPermissionOutcome::Cancelled,
                                    ),
                                );
                            }
                            None => break,
                        },
                    }
                }
                assert!(got_text, "no agent message chunks were received");
                Ok(agent_text)
                }
                .await;
                if let Ok(mut tx) = std::sync::Arc::try_unwrap(result_tx) { let _ = tx.send(r); }
                Ok(())
            },
        )
        .await;
        if let Err(err) = result {
            panic!("connect_and_run failed: {err:#}");
        }
    });

    match handle.await {
        Ok(()) => {}
        Err(join_err) => {
            match join_err.try_into_panic() {
                Ok(panic) => std::panic::resume_unwind(panic),
                Err(join_err) => panic!("test task failed to join: {join_err}"),
            }
        }
    }
    match result_rx.await {
        Ok(Ok(text)) => {
            assert!(
                text.to_uppercase().contains("OK"),
                "unexpected reply: {text:?}"
            );
        }
        Ok(Err(err)) => panic!("in-process round trip failed: {err:#}"),
        Err(_) => panic!("test task died"),
    }
}
