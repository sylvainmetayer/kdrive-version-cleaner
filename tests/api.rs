use std::time::Duration;

use kdvc::api::{Client, Depth};
use serde_json::json;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn client(server: &MockServer) -> Client {
    Client::with_options("tok".into(), server.uri(), Duration::ZERO).unwrap()
}

#[tokio::test]
async fn cursor_pagination_and_auth() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/3/drive/7/files/10/files"))
        .and(header("authorization", "Bearer tok"))
        .and(query_param("depth", "unlimited"))
        .and(query_param("cursor", "abc"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": "success",
            "data": [{"id": 3, "name": "b", "type": "file", "parent_id": 10}],
            "cursor": null, "has_more": false
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/3/drive/7/files/10/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": "success",
            "data": [{"id": 2, "name": "a", "type": "file", "parent_id": 10, "size": 5,
                      "version": {"is_multiple": true, "number": 3, "total_size": 15, "keep_forever": false}}],
            "cursor": "abc", "has_more": true
        })))
        .mount(&server)
        .await;

    let c = client(&server).await;
    let p1 = c.list_files(7, 10, Depth::Unlimited, None).await.unwrap();
    assert!(p1.has_more);
    assert_eq!(p1.items[0].version.as_ref().unwrap().number, 3);
    let p2 = c
        .list_files(7, 10, Depth::Unlimited, p1.cursor.as_deref())
        .await
        .unwrap();
    assert!(!p2.has_more);
    assert_eq!(p2.items[0].id, 3);
}

#[tokio::test]
async fn retries_after_429() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/2/drive"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/2/drive"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": "success", "data": [{"id": 1, "name": "Mon kDrive"}],
            "page": 1, "pages": 1
        })))
        .mount(&server)
        .await;

    let drives = client(&server).await.list_drives().await.unwrap();
    assert_eq!(drives[0].name, "Mon kDrive");
}

#[tokio::test]
async fn versions_page_pagination() {
    let server = MockServer::start().await;
    for page in 1..=2 {
        Mock::given(method("GET"))
            .and(path("/3/drive/7/files/2/versions"))
            .and(query_param("page", page.to_string()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": "success",
                "data": [{"id": page, "size": 10, "created_at": page * 100, "keep_forever": false}],
                "page": page, "pages": 2, "total": 2
            })))
            .mount(&server)
            .await;
    }
    let versions = client(&server).await.list_versions(7, 2).await.unwrap();
    assert_eq!(
        versions.iter().map(|v| v.id).collect::<Vec<_>>(),
        vec![1, 2]
    );
}

#[tokio::test]
async fn api_error_is_reported() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/2/drive/7/files/2/versions/9"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "result": "error", "error": {"code": "forbidden", "description": "no right"}
        })))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/2/drive/7/files/2/versions/8"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"result": "success", "data": true})),
        )
        .mount(&server)
        .await;

    let c = client(&server).await;
    let err = c.delete_version(7, 2, 9).await.unwrap_err().to_string();
    assert!(err.contains("forbidden"), "{err}");
    c.delete_version(7, 2, 8).await.unwrap();
}

#[tokio::test]
async fn versions_cursor_pagination() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/3/drive/7/files/2/versions"))
        .and(query_param("cursor", "next"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": "success",
            "data": [{"id": 2, "size": 10, "created_at": 200, "keep_forever": false}],
            "cursor": null, "has_more": false
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/3/drive/7/files/2/versions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": "success",
            "data": [{"id": 1, "size": 10, "created_at": 100, "keep_forever": false}],
            "cursor": "next", "has_more": true
        })))
        .mount(&server)
        .await;
    let versions = client(&server).await.list_versions(7, 2).await.unwrap();
    assert_eq!(
        versions.iter().map(|v| v.id).collect::<Vec<_>>(),
        vec![1, 2]
    );
}

#[tokio::test]
async fn delete_is_not_replayed_after_server_error() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/2/drive/7/files/2/versions/9"))
        .respond_with(ResponseTemplate::new(502).set_body_json(json!({
            "result": "error", "error": {"code": "bad_gateway"}
        })))
        .expect(1)
        .mount(&server)
        .await;
    let err = client(&server)
        .await
        .delete_version(7, 2, 9)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("502"), "{err}");
}

#[tokio::test]
async fn inconsistent_pagination_is_an_error() {
    let server = MockServer::start().await;
    // has_more sans curseur
    Mock::given(method("GET"))
        .and(path("/3/drive/7/files/10/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": "success", "data": [], "cursor": null, "has_more": true
        })))
        .mount(&server)
        .await;
    // curseur répété
    Mock::given(method("GET"))
        .and(path("/3/drive/7/files/2/versions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": "success", "data": [{"id": 1}], "cursor": "same", "has_more": true
        })))
        .mount(&server)
        .await;

    let c = client(&server).await;
    let err = c
        .list_files(7, 10, Depth::Unlimited, None)
        .await
        .err()
        .unwrap();
    assert!(err.to_string().contains("sans curseur"), "{err}");
    let err = c.list_versions(7, 2).await.unwrap_err();
    assert!(err.to_string().contains("curseur répété"), "{err}");
}

#[tokio::test]
async fn versions_page_pagination_with_has_more() {
    let server = MockServer::start().await;
    for page in 1..=2 {
        Mock::given(method("GET"))
            .and(path("/3/drive/7/files/2/versions"))
            .and(query_param("page", page.to_string()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": "success",
                "data": [{"id": page, "size": 10, "created_at": page * 100}],
                "page": page, "pages": 2, "has_more": page < 2, "cursor": null
            })))
            .mount(&server)
            .await;
    }
    let versions = client(&server).await.list_versions(7, 2).await.unwrap();
    assert_eq!(
        versions.iter().map(|v| v.id).collect::<Vec<_>>(),
        vec![1, 2]
    );
}

#[tokio::test]
async fn versions_cursor_cycle_is_an_error() {
    let server = MockServer::start().await;
    for (from, to) in [("a", "b"), ("b", "a")] {
        Mock::given(method("GET"))
            .and(path("/3/drive/7/files/2/versions"))
            .and(query_param("cursor", from))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": "success", "data": [{"id": 1}], "cursor": to, "has_more": true
            })))
            .mount(&server)
            .await;
    }
    Mock::given(method("GET"))
        .and(path("/3/drive/7/files/2/versions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": "success", "data": [{"id": 1}], "cursor": "a", "has_more": true
        })))
        .mount(&server)
        .await;
    let err = client(&server).await.list_versions(7, 2).await.unwrap_err();
    assert!(err.to_string().contains("curseur répété"), "{err}");
}
