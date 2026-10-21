//! A decision model behind the System One API, such as Jev: asked one typed question about the
//! request, whose probabilities decide it.

use std::sync::OnceLock;
use std::time::Duration;

use serde_json::{Value, json};
use url::Url;
use wf_harness::Permission;

use super::{DeciderRequest, Outcome};
use crate::machine_config::QuestionConfig;

const INSTRUCTIONS: &str = "A coding agent works on the item in the state, in its own \
    directory, and asks to make the tool call in `tool`. Should it be allowed? Follow the \
    guidance in the state when there is one.";

/// Where a decision model is and how it is asked.
pub(super) struct ModelDecider<'a> {
    pub url: &'a Url,
    pub model: &'a str,
    pub api_key_env: &'a str,
    pub question: QuestionConfig,
    pub timeout: Duration,
}

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        // reqwest comes without a crypto provider; ring is the one the rest of the build uses.
        let _ = rustls::crypto::ring::default_provider().install_default();
        reqwest::Client::new()
    })
}

impl ModelDecider<'_> {
    pub(super) async fn ask(&self, request: &DeciderRequest) -> Outcome {
        let passed = |why: String| Outcome::Passed { why };
        let Ok(key) = std::env::var(self.api_key_env) else {
            return passed(format!("`{}` is not set", self.api_key_env));
        };
        let body = json!({
            "model": self.model,
            "state": request,
            "questions": { "permission": question(self.question) },
        });
        let sent = client()
            .post(self.url.clone())
            .bearer_auth(key)
            .timeout(self.timeout)
            .json(&body)
            .send()
            .await;
        let response = match sent {
            Ok(response) => response,
            Err(error) if error.is_timeout() => {
                return passed(format!(
                    "it did not answer within {}",
                    humantime::format_duration(self.timeout)
                ));
            }
            Err(error) => return passed(format!("it could not be reached: {error}")),
        };
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return passed(format!("it answered {status}: {}", first_line(&text)));
        }
        match serde_json::from_str::<Value>(&text) {
            Ok(answer) => decide(self.question, &answer),
            Err(error) => passed(format!("its answer does not parse: {error}")),
        }
    }
}

fn question(question: QuestionConfig) -> Value {
    match question {
        QuestionConfig::Noul { .. } => json!({
            "type": "noul",
            "instructions": INSTRUCTIONS,
            "criteria": {
                "true": "Allow: it is safe, serves the item and fits the guidance.",
                "false": "Do not allow: it is risky, reaches outside the agent's directory, \
                          cannot be undone or goes against the guidance.",
            },
        }),
        QuestionConfig::Choice { .. } => json!({
            "type": "choice",
            "instructions": INSTRUCTIONS,
            "criteria": {
                "allow": "It is safe, serves the item and fits the guidance.",
                "deny": "It is clearly against the guidance or harmful.",
                "ask": "The principal should decide: it is unclear, or its consequences are hard to undo.",
            },
        }),
    }
}

/// Turns the answer into an outcome. Cloudflare wraps it in `{"result": ...}`.
fn decide(question: QuestionConfig, response: &Value) -> Outcome {
    let answers = match response.get("answers") {
        Some(answers) => answers,
        None => &response["result"]["answers"],
    };
    let answer = &answers["permission"];
    match question {
        QuestionConfig::Noul {
            allow_above,
            deny_below,
        } => {
            let Some(probability) = answer["noul"].as_f64() else {
                return Outcome::Passed {
                    why: "its answer has no probability".to_string(),
                };
            };
            let reason = format!("{probability:.2} that it should be allowed");
            if probability > allow_above {
                Outcome::Decided {
                    permission: Permission::Allow,
                    reason: Some(reason),
                }
            } else if probability < deny_below {
                Outcome::Decided {
                    permission: Permission::Deny,
                    reason: Some(reason),
                }
            } else {
                Outcome::Passed { why: reason }
            }
        }
        QuestionConfig::Choice { min_confidence } => {
            let (Some(choice), Some(confidence)) =
                (answer["choice"].as_str(), answer["confidence"].as_f64())
            else {
                return Outcome::Passed {
                    why: "its answer has no choice and confidence".to_string(),
                };
            };
            let reason = format!("{choice}, with confidence {confidence:.2}");
            let permission = match choice {
                "allow" => Permission::Allow,
                "deny" => Permission::Deny,
                _ => return Outcome::Passed { why: reason },
            };
            if confidence >= min_confidence {
                Outcome::Decided {
                    permission,
                    reason: Some(reason),
                }
            } else {
                Outcome::Passed { why: reason }
            }
        }
    }
}

fn first_line(text: &str) -> &str {
    let line = text.lines().next().unwrap_or("").trim();
    match line.char_indices().nth(200) {
        Some((end, _)) => &line[..end],
        None => line,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const NOUL: QuestionConfig = QuestionConfig::Noul {
        allow_above: 0.9,
        deny_below: 0.1,
    };
    const CHOICE: QuestionConfig = QuestionConfig::Choice {
        min_confidence: 0.8,
    };

    fn noul(probability: f64) -> Value {
        json!({"answers": {"permission": {"type": "noul", "noul": probability}}})
    }

    #[test]
    fn a_probability_allows_denies_or_passes_on() {
        assert!(matches!(
            decide(NOUL, &noul(0.95)),
            Outcome::Decided {
                permission: Permission::Allow,
                ..
            }
        ));
        assert!(matches!(
            decide(NOUL, &noul(0.05)),
            Outcome::Decided {
                permission: Permission::Deny,
                ..
            }
        ));
        assert_eq!(
            decide(NOUL, &noul(0.5)),
            Outcome::Passed {
                why: "0.50 that it should be allowed".into()
            }
        );
    }

    #[test]
    fn a_choice_is_taken_when_the_model_is_sure() {
        let choice = |choice: &str, confidence: f64| {
            json!({"answers": {"permission": {
                "type": "choice", "choice": choice, "confidence": confidence,
                "probabilities": {},
            }}})
        };
        assert!(matches!(
            decide(CHOICE, &choice("deny", 0.9)),
            Outcome::Decided {
                permission: Permission::Deny,
                ..
            }
        ));
        assert!(matches!(
            decide(CHOICE, &choice("allow", 0.5)),
            Outcome::Passed { .. }
        ));
        assert!(matches!(
            decide(CHOICE, &choice("ask", 0.99)),
            Outcome::Passed { .. }
        ));
    }

    #[test]
    fn a_cloudflare_answer_is_read_from_its_result() {
        let wrapped = json!({"result": noul(0.99), "success": true});
        assert!(matches!(
            decide(NOUL, &wrapped),
            Outcome::Decided {
                permission: Permission::Allow,
                ..
            }
        ));
    }

    /// Serves one HTTP request with `answer` and hands back what was asked.
    async fn serve_once(answer: Value) -> (Url, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!(
            "http://{}/v1/systemone",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let task = crate::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut asked = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                let read = stream.read(&mut buffer).await.unwrap();
                asked.extend_from_slice(&buffer[..read]);
                let text = String::from_utf8_lossy(&asked);
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let length: usize = head
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .map(|value| value.trim().parse().unwrap())
                        })
                        .unwrap_or(0);
                    if body.len() >= length {
                        break;
                    }
                }
            }
            let body = answer.to_string();
            let reply = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(reply.as_bytes()).await.unwrap();
            String::from_utf8(asked).unwrap()
        });
        (url, task)
    }

    #[tokio::test]
    async fn the_model_is_asked_over_the_system_one_api() {
        let (url, asked) = serve_once(noul(0.97)).await;
        // SAFETY: tests that read this variable run in this process only here.
        unsafe { std::env::set_var("WF_TEST_DECISION_KEY", "secret") };
        let decider = ModelDecider {
            url: &url,
            model: "jev-latest",
            api_key_env: "WF_TEST_DECISION_KEY",
            question: NOUL,
            timeout: Duration::from_secs(5),
        };
        let outcome = decider.ask(&crate::decide::tests::request()).await;
        assert!(
            matches!(
                outcome,
                Outcome::Decided {
                    permission: Permission::Allow,
                    ..
                }
            ),
            "{outcome:?}"
        );
        let asked = asked.await.unwrap();
        assert!(asked.starts_with("POST /v1/systemone"), "{asked}");
        assert!(
            asked
                .to_ascii_lowercase()
                .contains("authorization: bearer secret")
        );
        let body: Value = serde_json::from_str(asked.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body["model"], "jev-latest");
        assert_eq!(body["questions"]["permission"]["type"], "noul");
        assert_eq!(body["state"]["tool"]["title"], "cargo test");
    }

    /// Reaches TypeSafe over TLS; a made-up key is refused. Run with `--ignored`.
    #[tokio::test]
    #[ignore = "needs the network"]
    async fn typesafe_is_reached_over_tls() {
        // SAFETY: tests that read this variable run in this process only here.
        unsafe { std::env::set_var("WF_TEST_MADE_UP_KEY", "not-a-key") };
        let url = Url::parse("https://api.typesafe.ai/v1/systemone").unwrap();
        let decider = ModelDecider {
            url: &url,
            model: "jev-latest",
            api_key_env: "WF_TEST_MADE_UP_KEY",
            question: NOUL,
            timeout: Duration::from_secs(20),
        };
        let outcome = decider.ask(&crate::decide::tests::request()).await;
        let Outcome::Passed { why } = outcome else {
            panic!("{outcome:?}")
        };
        assert!(why.starts_with("it answered 401"), "{why}");
    }

    #[tokio::test]
    async fn a_missing_key_passes_on() {
        let url = Url::parse("http://127.0.0.1:9/").unwrap();
        let decider = ModelDecider {
            url: &url,
            model: "jev-latest",
            api_key_env: "WF_TEST_NO_SUCH_KEY",
            question: NOUL,
            timeout: Duration::from_secs(1),
        };
        assert_eq!(
            decider.ask(&crate::decide::tests::request()).await,
            Outcome::Passed {
                why: "`WF_TEST_NO_SUCH_KEY` is not set".into()
            }
        );
    }
}
