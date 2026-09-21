#[path = "../src/peer_auth.rs"]
mod peer_auth;

use std::path::Path;
use std::time::Duration;

use peer_auth::{
    AuthStore, AuthTtls, AuthenticateRequest, Challenge, ChallengeRequest, DeviceIdentity, Invite,
    Principal, RedeemRequest, TokenResponse,
};
use reqwest::{Client, StatusCode};
use tempfile::TempDir;

struct Server {
    url: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn protected_principal(
    axum::Extension(principal): axum::Extension<kratos_sync::peer::PeerPrincipal>,
) -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "profileId": principal.profile_id,
        "deviceId": principal.device_id,
    }))
}

async fn serve(store: AuthStore) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    let protected = axum::Router::new()
        .route("/protected", axum::routing::get(protected_principal))
        .route_layer(axum::middleware::from_fn_with_state(
            store.clone(),
            peer_auth::require_peer_principal,
        ));
    let app = peer_auth::router(store).merge(protected);
    let task = tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
    Server {
        url: format!("http://{address}"),
        task,
    }
}

fn identity(dir: &TempDir, name: &str) -> DeviceIdentity {
    DeviceIdentity::load_or_create(dir.path().join(name)).expect("identity")
}

async fn challenge(client: &Client, server: &Server, principal: &Principal) -> reqwest::Response {
    client
        .post(format!("{}/pair/challenge", server.url))
        .json(&ChallengeRequest {
            profile_id: principal.profile_id.clone(),
            device_id: principal.device_id.clone(),
        })
        .send()
        .await
        .expect("challenge response")
}

async fn login(
    client: &Client,
    server: &Server,
    principal: &Principal,
    identity: &DeviceIdentity,
) -> TokenResponse {
    let response = challenge(client, server, principal).await;
    assert_eq!(response.status(), StatusCode::OK);
    let challenge: Challenge = response.json().await.expect("challenge json");
    let proof = identity.sign_challenge(&challenge).expect("sign challenge");
    let response = client
        .post(format!("{}/pair/authenticate", server.url))
        .json(&proof)
        .send()
        .await
        .expect("authenticate response");
    assert_eq!(response.status(), StatusCode::OK);
    response.json().await.expect("token json")
}

async fn create_invite(client: &Client, server: &Server, token: &str) -> Invite {
    let response = client
        .post(format!("{}/pair/invite", server.url))
        .bearer_auth(token)
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("invite response");
    assert_eq!(response.status(), StatusCode::OK);
    response.json().await.expect("invite json")
}

async fn redeem(
    client: &Client,
    server: &Server,
    identity: &DeviceIdentity,
    invite: &Invite,
) -> (reqwest::Response, RedeemRequest) {
    let proof = identity
        .redeem_request(invite, Some("paired device".into()))
        .expect("sign invite");
    let response = client
        .post(format!("{}/pair/redeem", server.url))
        .json(&proof)
        .send()
        .await
        .expect("redeem response");
    (response, proof)
}

#[tokio::test]
async fn http_pairing_uses_real_proofs_and_enforces_replay_revocation_and_isolation() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = AuthStore::open(dir.path().join("auth.sqlite")).expect("store");
    let owner = identity(&dir, "owner-key.json");
    let member = identity(&dir, "member-key.json");
    let other_owner = identity(&dir, "other-owner-key.json");
    let owner_principal = store
        .create_profile(&owner.public_key(), Some("owner"))
        .expect("profile");
    let other_profile = store
        .create_profile(&other_owner.public_key(), Some("other owner"))
        .expect("other profile");
    let mut revocations = store.subscribe_revocations();
    let server = serve(store.clone()).await;
    let client = Client::new();

    let owner_token = login(&client, &server, &owner_principal, &owner).await;
    let invite = create_invite(&client, &server, &owner_token.token).await;
    assert_eq!(invite.profile_id, owner_principal.profile_id);

    let (response, proof) = redeem(&client, &server, &member, &invite).await;
    assert_eq!(response.status(), StatusCode::OK);
    let paired: Principal = response.json().await.expect("paired principal");
    assert_eq!(paired.device_id, member.device_id());

    let replay = client
        .post(format!("{}/pair/redeem", server.url))
        .json(&proof)
        .send()
        .await
        .expect("replay response");
    assert_eq!(replay.status(), StatusCode::CONFLICT);

    let member_token = login(&client, &server, &paired, &member).await;
    assert_eq!(
        store
            .authenticate(&member_token.token)
            .expect("valid token"),
        paired
    );

    let protected = client
        .get(format!("{}/protected", server.url))
        .bearer_auth(&member_token.token)
        .send()
        .await
        .expect("protected response");
    assert_eq!(protected.status(), StatusCode::OK);
    assert_eq!(
        protected
            .json::<serde_json::Value>()
            .await
            .expect("protected principal")["deviceId"],
        paired.device_id
    );

    let forbidden = client
        .post(format!("{}/pair/invite", server.url))
        .bearer_auth(&member_token.token)
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("member invite response");
    assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);

    let isolated = challenge(
        &client,
        &server,
        &Principal {
            profile_id: other_profile.profile_id,
            device_id: paired.device_id.clone(),
        },
    )
    .await;
    assert_eq!(isolated.status(), StatusCode::UNAUTHORIZED);

    let challenge_response = challenge(&client, &server, &paired).await;
    let auth_challenge: Challenge = challenge_response.json().await.expect("challenge");
    let auth_proof = member.sign_challenge(&auth_challenge).expect("proof");
    let first = client
        .post(format!("{}/pair/authenticate", server.url))
        .json(&auth_proof)
        .send()
        .await
        .expect("first authentication");
    assert_eq!(first.status(), StatusCode::OK);
    let replay = client
        .post(format!("{}/pair/authenticate", server.url))
        .json(&auth_proof)
        .send()
        .await
        .expect("authentication replay");
    assert_eq!(replay.status(), StatusCode::CONFLICT);

    let revoke = client
        .post(format!("{}/pair/revoke", server.url))
        .bearer_auth(&owner_token.token)
        .json(&serde_json::json!({"deviceId": paired.device_id}))
        .send()
        .await
        .expect("revoke response");
    assert_eq!(revoke.status(), StatusCode::NO_CONTENT);
    assert!(store.authenticate(&member_token.token).is_err());

    let denied = client
        .get(format!("{}/protected", server.url))
        .bearer_auth(&member_token.token)
        .send()
        .await
        .expect("revoked protected response");
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    let event = tokio::time::timeout(Duration::from_secs(1), revocations.recv())
        .await
        .expect("revocation notification")
        .expect("open notification channel");
    assert_eq!(event.profile_id, owner_principal.profile_id);
    assert_eq!(event.device_id, paired.device_id);
    assert_eq!(
        challenge(&client, &server, &paired).await.status(),
        StatusCode::UNAUTHORIZED
    );

    let oversized = client
        .post(format!("{}/pair/redeem", server.url))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(format!("{{\"junk\":\"{}\"}}", "x".repeat(20_000)))
        .send()
        .await
        .expect("oversized response");
    assert_eq!(oversized.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn malformed_cross_bound_proofs_are_rejected_without_consuming_credentials() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = AuthStore::open(dir.path().join("auth.sqlite")).expect("store");
    let owner = identity(&dir, "owner.json");
    let member = identity(&dir, "member.json");
    let principal = store
        .create_profile(&owner.public_key(), None)
        .expect("profile");
    let server = serve(store.clone()).await;
    let client = Client::new();
    let token = login(&client, &server, &principal, &owner).await;
    let invite = create_invite(&client, &server, &token.token).await;
    let mut bad_proof = member.redeem_request(&invite, None).expect("proof");
    bad_proof.public_key = owner
        .redeem_request(&invite, None)
        .expect("owner proof")
        .public_key;
    let rejected = client
        .post(format!("{}/pair/redeem", server.url))
        .json(&bad_proof)
        .send()
        .await
        .expect("rejected proof");
    assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);

    let (valid, _) = redeem(&client, &server, &member, &invite).await;
    assert_eq!(
        valid.status(),
        StatusCode::OK,
        "invalid proof must not consume invite"
    );
    let paired: Principal = valid.json().await.expect("paired");
    let auth_challenge: Challenge = challenge(&client, &server, &paired)
        .await
        .json()
        .await
        .expect("challenge");
    let mut wrong_profile: AuthenticateRequest = member
        .sign_challenge(&auth_challenge)
        .expect("authentication proof");
    wrong_profile.profile_id = UuidForTest::new();
    let response = client
        .post(format!("{}/pair/authenticate", server.url))
        .json(&wrong_profile)
        .send()
        .await
        .expect("cross-profile response");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

struct UuidForTest;
impl UuidForTest {
    fn new() -> String {
        uuid::Uuid::new_v4().to_string()
    }
}

#[tokio::test]
async fn expiration_is_enforced_for_invites_challenges_and_tokens() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = AuthStore::open_with_ttls(
        dir.path().join("auth.sqlite"),
        AuthTtls {
            invite: Duration::from_secs(1),
            challenge: Duration::from_secs(1),
            token: Duration::from_secs(1),
        },
    )
    .expect("store");
    let owner = identity(&dir, "owner.json");
    let member = identity(&dir, "member.json");
    let principal = store
        .create_profile(&owner.public_key(), None)
        .expect("profile");
    let server = serve(store.clone()).await;
    let client = Client::new();

    let initial_token = login(&client, &server, &principal, &owner).await;
    let invite = create_invite(&client, &server, &initial_token.token).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        redeem(&client, &server, &member, &invite).await.0.status(),
        StatusCode::CONFLICT
    );

    let challenge: Challenge = challenge(&client, &server, &principal)
        .await
        .json()
        .await
        .expect("challenge");
    let proof = owner.sign_challenge(&challenge).expect("proof");
    tokio::time::sleep(Duration::from_secs(2)).await;
    let expired = client
        .post(format!("{}/pair/authenticate", server.url))
        .json(&proof)
        .send()
        .await
        .expect("expired challenge response");
    assert_eq!(expired.status(), StatusCode::CONFLICT);

    let token = login(&client, &server, &principal, &owner).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(store.authenticate(&token.token).is_err());
}

#[tokio::test]
async fn identities_and_tokens_survive_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let database = dir.path().join("auth.sqlite");
    let owner_path = dir.path().join("owner.json");
    let owner = DeviceIdentity::load_or_create(&owner_path).expect("owner identity");
    let owner_public = owner.public_key();
    let store = AuthStore::open(&database).expect("store");
    let principal = store
        .create_profile(&owner_public, Some("owner"))
        .expect("profile");
    let challenge = store
        .create_challenge(&principal.profile_id, &principal.device_id)
        .expect("challenge");
    let proof = owner.sign_challenge(&challenge).expect("signature");
    let token = store
        .authenticate_challenge(&proof)
        .expect("authentication");
    drop(store);

    let reloaded_owner = DeviceIdentity::load_or_create(&owner_path).expect("reload identity");
    assert_eq!(reloaded_owner.public_key(), owner_public);
    let reopened = AuthStore::open(&database).expect("reopen store");
    assert_eq!(
        reopened
            .authenticate(&token.token)
            .expect("persisted token"),
        principal
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(&owner_path)
                .expect("identity metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    assert_no_plaintext_secret(&database, &token.token);
}

#[test]
fn trusted_identity_can_repair_but_revoked_identity_requires_a_fresh_key() {
    let dir = tempfile::tempdir().unwrap();
    let store = AuthStore::open(dir.path().join("auth.sqlite")).unwrap();
    let owner = identity(&dir, "owner-repair.json");
    let member = identity(&dir, "member-repair.json");
    let fresh = identity(&dir, "fresh-recovery.json");
    let principal = store.create_profile(&owner.public_key(), None).unwrap();

    let invite = store
        .create_invite(&principal.profile_id, &principal.device_id)
        .unwrap();
    let paired = store
        .redeem_invite(&member.redeem_request(&invite, None).unwrap())
        .unwrap();
    let repair = store
        .create_invite(&principal.profile_id, &principal.device_id)
        .unwrap();
    assert_eq!(
        store
            .redeem_invite(&member.redeem_request(&repair, None).unwrap())
            .unwrap(),
        paired
    );

    store.revoke_device(&principal, &paired.device_id).unwrap();
    let denied = store
        .create_invite(&principal.profile_id, &principal.device_id)
        .unwrap();
    assert!(matches!(
        store.redeem_invite(&member.redeem_request(&denied, None).unwrap()),
        Err(peer_auth::AuthError::Revoked)
    ));
    let recovery = store
        .create_invite(&principal.profile_id, &principal.device_id)
        .unwrap();
    let recovered = store
        .redeem_invite(&fresh.redeem_request(&recovery, None).unwrap())
        .unwrap();
    assert_ne!(recovered.device_id, paired.device_id);
}

#[test]
fn lagged_revocation_receivers_can_reconcile_from_persistent_auth() {
    let dir = tempfile::tempdir().unwrap();
    let store = AuthStore::open(dir.path().join("auth.sqlite")).unwrap();
    let owner = identity(&dir, "owner-lag.json");
    let principal = store.create_profile(&owner.public_key(), None).unwrap();
    let mut receiver = store.subscribe_revocations();
    for index in 0..70 {
        let member = identity(&dir, &format!("lag-{index}.json"));
        let invite = store
            .create_invite(&principal.profile_id, &principal.device_id)
            .unwrap();
        let paired = store
            .redeem_invite(&member.redeem_request(&invite, None).unwrap())
            .unwrap();
        store.revoke_device(&principal, &paired.device_id).unwrap();
    }
    assert!(matches!(
        receiver.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_))
    ));
    assert_eq!(store.revoked_devices().unwrap().len(), 70);
}

fn assert_no_plaintext_secret(database: &Path, secret: &str) {
    let bytes = std::fs::read(database).expect("database bytes");
    assert!(
        !bytes
            .windows(secret.len())
            .any(|window| window == secret.as_bytes()),
        "plaintext bearer must not be persisted"
    );
}
