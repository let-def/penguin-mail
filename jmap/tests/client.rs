//! The client against a mock JMAP server: what it sends, and how it reads
//! what comes back.

use std::time::Duration;

use mailrs_jmap::{Jmap, JmapError};
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const MAIL: &str = "urn:ietf:params:jmap:mail";
const SUBMISSION: &str = "urn:ietf:params:jmap:submission";

fn session(server: &MockServer, state: &str, max_get: usize) -> Value {
    let base = server.uri();
    json!({
        "capabilities": {
            "urn:ietf:params:jmap:core": { "maxCallsInRequest": 8, "maxObjectsInGet": max_get,
                                           "maxObjectsInSet": 100, "maxSizeUpload": 1000 },
            MAIL: {}, SUBMISSION: {}
        },
        "accounts": { "u1": { "name": "dana@fastmail.com" } },
        "primaryAccounts": { MAIL: "u1", SUBMISSION: "u1" },
        "username": "dana@fastmail.com",
        "apiUrl": format!("{base}/api/"),
        "downloadUrl": format!("{base}/download/{{accountId}}/{{blobId}}/{{name}}?type={{type}}"),
        "uploadUrl": format!("{base}/upload/{{accountId}}/"),
        "state": state
    })
}

async fn server(max_get: usize) -> (MockServer, Jmap) {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/session"))
        .and(header("Authorization", "Bearer secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(session(&server, "s1", max_get)))
        .mount(&server)
        .await;
    let jmap = Jmap::new(&format!("{}/session", server.uri()), "secret").unwrap();
    (server, jmap)
}

/// Answers each method call of a request with `answer(name, args)`.
struct Echo(fn(&str, &Value) -> Value, &'static str);

impl Respond for Echo {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let answers: Vec<Value> = body["methodCalls"]
            .as_array()
            .unwrap()
            .iter()
            .map(|call| {
                let (name, args, id) = (call[0].as_str().unwrap(), &call[1], &call[2]);
                let answer = (self.0)(name, args);
                match answer.get("error") {
                    Some(error) => json!(["error", error, id]),
                    None => json!([name, answer, id]),
                }
            })
            .collect();
        ResponseTemplate::new(200)
            .set_body_json(json!({ "methodResponses": answers, "sessionState": self.1 }))
    }
}

#[tokio::test]
async fn calls_go_to_the_mail_account_with_the_capabilities_they_need() {
    let (server, jmap) = server(500).await;
    Mock::given(method("POST"))
        .and(path("/api/"))
        .and(body_partial_json(json!({
            "using": ["urn:ietf:params:jmap:core", MAIL],
            "methodCalls": [["Mailbox/get", { "accountId": "u1", "ids": null }, "c0"]]
        })))
        .respond_with(Echo(
            |_, _| json!({ "state": "m1", "list": [
                { "id": "mb1", "name": "Inbox", "role": "inbox", "totalEmails": 3 },
                { "id": "mb2", "name": "Receipts", "parentId": null }
            ], "notFound": [] }),
            "s1",
        ))
        .expect(2)
        .mount(&server)
        .await;

    let got = jmap.mailboxes().await.unwrap();
    assert_eq!(got.state, "m1");
    assert_eq!(got.list[0].role.as_deref(), Some("inbox"));
    assert_eq!(got.list[0].total_emails, 3);
    assert!(got.list[1].is_subscribed, "a server that leaves it out subscribes");
    jmap.mailboxes().await.unwrap();
    let reads = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/session")
        .count();
    assert_eq!(reads, 1, "the session is read once");
}

#[tokio::test]
async fn a_new_session_state_reads_the_session_again() {
    let (server, jmap) = server(500).await;
    Mock::given(method("POST"))
        .and(path("/api/"))
        .respond_with(Echo(|_, _| json!({ "state": "m1", "list": [] }), "s2"))
        .mount(&server)
        .await;
    jmap.mailboxes().await.unwrap();
    jmap.mailboxes().await.unwrap();
    let reads = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/session")
        .count();
    assert_eq!(reads, 2);
}

#[tokio::test]
async fn a_method_error_comes_back_as_its_kind() {
    let (server, jmap) = server(500).await;
    Mock::given(method("POST"))
        .and(path("/api/"))
        .respond_with(Echo(
            |_, _| json!({ "error": { "type": "cannotCalculateChanges" } }),
            "s1",
        ))
        .mount(&server)
        .await;
    assert_eq!(jmap.email_changes("old", 100).await, Err(JmapError::StateLost));
}

#[tokio::test]
async fn a_refused_token_and_a_throttle_say_so() {
    let (server, jmap) = server(500).await;
    Mock::given(method("POST"))
        .and(path("/api/"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "7"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    assert_eq!(
        jmap.mailboxes().await,
        Err(JmapError::Throttled { retry_after: Some(Duration::from_secs(7)) })
    );
    assert_eq!(jmap.mailboxes().await, Err(JmapError::Unauthorized));
}

#[tokio::test]
async fn a_get_of_more_ids_than_the_server_takes_splits_inside_one_request() {
    let (server, jmap) = server(2).await;
    Mock::given(method("POST"))
        .and(path("/api/"))
        .respond_with(Echo(
            |_, args| {
                let ids = args["ids"].as_array().unwrap();
                assert!(ids.len() <= 2, "{ids:?}");
                assert!(args["properties"].as_array().unwrap().contains(&json!("keywords")));
                let list: Vec<Value> = ids
                    .iter()
                    .filter(|id| *id != "gone")
                    .map(|id| json!({ "id": id, "threadId": "t", "keywords": { "$seen": true } }))
                    .collect();
                let missing: Vec<&Value> = ids.iter().filter(|id| *id == "gone").collect();
                json!({ "state": "e1", "list": list, "notFound": missing })
            },
            "s1",
        ))
        .expect(1)
        .mount(&server)
        .await;
    let ids: Vec<String> = ["a", "b", "gone", "d", "e"].map(String::from).to_vec();
    let got = jmap.emails(&ids).await.unwrap();
    assert_eq!(got.list.len(), 4);
    assert_eq!(got.not_found, ["gone"]);
    assert!(got.list[0].keywords.contains_key("$seen"));
}

#[tokio::test]
async fn sending_files_a_draft_and_submits_it_in_one_request() {
    let (server, jmap) = server(500).await;
    Mock::given(method("POST"))
        .and(path("/upload/u1/"))
        .and(header("Content-Type", "message/rfc822"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "accountId": "u1", "blobId": "B1", "type": "message/rfc822", "size": 5
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/"))
        .and(body_partial_json(json!({ "using": ["urn:ietf:params:jmap:core", MAIL, SUBMISSION] })))
        .respond_with(Echo(
            |name, args| match name {
                "Email/import" => {
                    assert_eq!(args["emails"]["m"]["blobId"], "B1");
                    assert_eq!(args["emails"]["m"]["mailboxIds"], json!({ "drafts": true }));
                    json!({ "created": { "m": { "id": "E1", "threadId": "T1", "blobId": "B1" } } })
                }
                "EmailSubmission/set" => {
                    assert_eq!(args["create"]["s"]["emailId"], "#m");
                    assert_eq!(args["onSuccessUpdateEmail"]["#s"]["mailboxIds/sent"], true);
                    json!({ "created": { "s": { "id": "S1" } } })
                }
                other => panic!("{other}"),
            },
            "s1",
        ))
        .mount(&server)
        .await;
    let sent = jmap.send(b"hello".to_vec(), "id1", "drafts", "sent").await.unwrap();
    assert_eq!(sent.email_id, "E1");
    assert_eq!(sent.thread_id.as_deref(), Some("T1"));
}

#[tokio::test]
async fn a_refused_submission_is_an_error() {
    let (server, jmap) = server(500).await;
    Mock::given(method("POST"))
        .and(path("/upload/u1/"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "blobId": "B1" })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/"))
        .respond_with(Echo(
            |name, _| match name {
                "Email/import" => json!({ "created": { "m": { "id": "E1" } } }),
                _ => json!({ "notCreated": { "s": { "type": "forbiddenFrom", "description": "not yours" } } }),
            },
            "s1",
        ))
        .mount(&server)
        .await;
    assert_eq!(
        jmap.send(b"x".to_vec(), "id1", "drafts", "sent").await,
        Err(JmapError::Method { kind: "forbiddenFrom".into(), description: "not yours".into() })
    );
}

#[tokio::test]
async fn a_blob_comes_from_the_session_template_and_stops_at_the_limit() {
    let (server, jmap) = server(500).await;
    Mock::given(method("GET"))
        .and(path("/download/u1/B%201/blob"))
        .and(header("Authorization", "Bearer secret"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"0123456789".to_vec()))
        .mount(&server)
        .await;
    assert_eq!(jmap.raw("B 1", 100).await.unwrap(), b"0123456789");
    assert_eq!(jmap.raw("B 1", 4).await, Err(JmapError::TooLarge { limit: 4 }));
}

#[tokio::test]
async fn contact_calls_go_to_the_contacts_account() {
    let server = MockServer::start().await;
    let base = server.uri();
    Mock::given(method("GET"))
        .and(path("/session"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "capabilities": { "urn:ietf:params:jmap:core": {}, "urn:ietf:params:jmap:contacts": {} },
            "primaryAccounts": { "urn:ietf:params:jmap:contacts": "c1" },
            "username": "dana@fastmail.com",
            "apiUrl": format!("{base}/api/"), "downloadUrl": format!("{base}/d/{{accountId}}/{{blobId}}"),
            "uploadUrl": format!("{base}/u/"), "state": "s1"
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/"))
        .and(body_partial_json(json!({
            "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:contacts"],
            "methodCalls": [["ContactCard/get", { "accountId": "c1", "ids": ["k1"] }, "c0"]]
        })))
        .respond_with(Echo(
            |_, _| json!({ "state": "cs", "list": [{ "id": "k1", "name": { "full": "Ann" } }] }),
            "s1",
        ))
        .mount(&server)
        .await;
    let jmap = Jmap::new(&format!("{base}/session"), "secret").unwrap();
    let got = jmap.cards(&["k1".to_string()]).await.unwrap();
    assert_eq!(got.list[0]["name"]["full"], "Ann");
    assert_eq!(jmap.mailboxes().await, Err(JmapError::Missing("urn:ietf:params:jmap:mail")));
}
