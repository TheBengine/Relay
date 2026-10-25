//! Friends groups: joined by explicit invitation and acceptance. The people in
//! a group need not be friends with each other; being together gives them a
//! group to chat in and nothing else.
use super::*;
use crate::tests::request;
use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

const CIPHERTEXT: &str = "relay-e2ee:v1:AAAA";

/// A request with a raw body and a raw answer, for uploads and downloads.
async fn raw(
    app: &Router,
    method: &str,
    uri: &str,
    token: &str,
    body: &[u8],
) -> (StatusCode, Vec<u8>) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(body.to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, bytes.to_vec())
}

struct Friends {
    _data: tempfile::TempDir,
    state: AppState,
    app: Router,
}

impl Friends {
    fn new() -> Self {
        let data = tempfile::tempdir().unwrap();
        let state = AppState::open_with_config(data.path(), "Relay friends", 0, config()).unwrap();
        let app = router(state.clone());
        Self {
            _data: data,
            state,
            app,
        }
    }
    fn people(&self, names: &[&str]) -> Vec<Session> {
        names.iter().map(|n| account(&self.state, n)).collect()
    }
    async fn befriend(&self, x: &Session, y: &Session) {
        for (from, to) in [(x, y), (y, x)] {
            let (status, _) = request(
                &self.app,
                "POST",
                &format!("/api/friends/{}", to.user.id),
                Some(&from.token),
                Value::Null,
            )
            .await;
            assert_eq!(status, StatusCode::OK);
        }
    }
    /// The owner creates a group and names the friends to invite.
    async fn create(
        &self,
        owner: &Session,
        name: &str,
        others: &[&Session],
    ) -> (StatusCode, Value) {
        let ids: Vec<_> = others.iter().map(|o| o.user.id.clone()).collect();
        request(
            &self.app,
            "POST",
            "/api/conversations",
            Some(&owner.token),
            json!({"name":name,"kind":"group","member_ids":ids}),
        )
        .await
    }
    async fn invite(&self, owner: &Session, channel: &str, who: &Session) -> StatusCode {
        request(
            &self.app,
            "POST",
            &format!("/api/conversations/{channel}/invitations"),
            Some(&owner.token),
            json!({"user_id": who.user.id}),
        )
        .await
        .0
    }
    async fn answer(&self, who: &Session, channel: &str, action: &str) -> StatusCode {
        request(
            &self.app,
            "POST",
            &format!("/api/group-invitations/{channel}/{action}"),
            Some(&who.token),
            Value::Null,
        )
        .await
        .0
    }
    /// A group whose invited friends have all accepted.
    async fn group(&self, owner: &Session, name: &str, others: &[&Session]) -> String {
        let (status, made) = self.create(owner, name, others).await;
        assert_eq!(status, StatusCode::OK, "{made}");
        let channel = made["channel_id"].as_str().unwrap().to_owned();
        for other in others {
            assert_eq!(self.answer(other, &channel, "accept").await, StatusCode::OK);
        }
        channel
    }
    async fn boot(&self, who: &Session) -> Value {
        request(
            &self.app,
            "GET",
            "/api/bootstrap",
            Some(&who.token),
            Value::Null,
        )
        .await
        .1
    }
    async fn members(&self, channel: &str) -> Vec<String> {
        conversations::info(&lock(&self.state.0.db), channel)
            .unwrap()
            .map(|c| c.member_ids)
            .unwrap_or_default()
    }
    fn owner(&self, channel: &str) -> String {
        conversations::info(&lock(&self.state.0.db), channel)
            .unwrap()
            .unwrap()
            .owner_id
    }
    fn can(&self, channel: &str, user: &Session, permission: u32) -> bool {
        channel_access(&lock(&self.state.0.db), channel, &user.user.id, permission).is_ok()
    }
    async fn profile(&self, viewer: &Session, person: &Session) -> StatusCode {
        request(
            &self.app,
            "GET",
            &format!("/api/profiles/{}", person.user.id),
            Some(&viewer.token),
            Value::Null,
        )
        .await
        .0
    }
    async fn waiting(&self, who: &Session) -> Vec<Value> {
        self.boot(who).await["group_invitations"]
            .as_array()
            .unwrap()
            .clone()
    }
    async fn sees_member(&self, viewer: &Session, person: &Session) -> bool {
        self.boot(viewer).await["members"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == person.user.id)
    }
}

fn sorted(mut ids: Vec<String>) -> Vec<String> {
    ids.sort();
    ids
}

#[tokio::test]
async fn friends_who_do_not_know_each_other_join_one_group_only_by_accepting() {
    let f = Friends::new();
    let p = f.people(&["alice", "bob", "cara", "dave"]);
    let (a, b, c, d) = (&p[0], &p[1], &p[2], &p[3]);
    // Alice is friends with Bob, Cara and Dave; Bob and Cara have never met.
    for other in [b, c, d] {
        f.befriend(a, other).await;
    }
    let boot = f.boot(a).await;
    assert!(boot["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .any(|cap| cap == relay_protocol::PEOPLE_GROUP_INVITES_CAPABILITY));
    let (status, made) = f.create(a, "Weekend", &[b, c]).await;
    assert_eq!(status, StatusCode::OK, "{made}");
    let channel = made["channel_id"].as_str().unwrap().to_owned();
    // Nobody is added by being invited: only Alice is in the group.
    assert_eq!(f.members(&channel).await, vec![a.user.id.clone()]);
    let sent = f.boot(a).await["group_invitations_sent"].clone();
    assert_eq!(sent.as_array().unwrap().len(), 2);
    // Before accepting, an invitee sees who invited them and the name, and nothing else.
    let target = CryptoTarget {
        channel_id: Some(channel.clone()),
        peer_id: None,
    };
    for invitee in [b, c] {
        let waiting = f.waiting(invitee).await;
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0]["name"], "Weekend");
        assert_eq!(waiting[0]["inviter_id"], a.user.id);
        let boot = f.boot(invitee).await;
        assert!(boot["channels"].as_array().unwrap().is_empty());
        assert!(boot["conversations"].as_array().unwrap().is_empty());
        assert!(!f.can(&channel, invitee, READ));
        let (status, _) = request(
            &f.app,
            "GET",
            &format!("/api/messages?channel_id={channel}"),
            Some(&invitee.token),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = request(
            &f.app,
            "POST",
            "/api/messages",
            Some(&invitee.token),
            json!({"channel_id":channel,"content":CIPHERTEXT}),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(
            e2ee::roster(&f.state, &lock(&f.state.0.db), &invitee.user.id, &target).is_err(),
            "no encryption roster before accepting"
        );
    }
    assert!(!f.sees_member(b, c).await);
    assert_eq!(f.profile(b, c).await, StatusCode::FORBIDDEN);
    // Someone who was never invited cannot accept, and nothing changes for them.
    assert_eq!(f.answer(d, &channel, "accept").await, StatusCode::GONE);
    assert!(f.waiting(d).await.is_empty());
    // Bob accepts; Cara has not, so Bob still cannot see her.
    assert_eq!(f.answer(b, &channel, "accept").await, StatusCode::OK);
    assert_eq!(
        f.members(&channel).await,
        sorted(vec![a.user.id.clone(), b.user.id.clone()])
    );
    assert!(f.can(&channel, b, SEND));
    assert!(!f.sees_member(b, c).await);
    assert!(f.waiting(b).await.is_empty());
    // Cara accepts and now the group works for all three.
    assert_eq!(f.answer(c, &channel, "accept").await, StatusCode::OK);
    assert_eq!(f.members(&channel).await.len(), 3);
    let roster = |who: &Session| {
        e2ee::roster(&f.state, &lock(&f.state.0.db), &who.user.id, &target)
            .map(|(_, _, members)| sorted(members.into_iter().map(|m| m.id).collect()))
    };
    let everyone = sorted(vec![
        a.user.id.clone(),
        b.user.id.clone(),
        c.user.id.clone(),
    ]);
    for member in [a, b, c] {
        assert_eq!(roster(member).unwrap(), everyone);
    }
    assert!(roster(d).is_err());
    for (from, to) in [(c, b), (b, c)] {
        let (status, _) = request(
            &f.app,
            "POST",
            "/api/messages",
            Some(&from.token),
            json!({"channel_id":channel,"content":CIPHERTEXT}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, history) = request(
            &f.app,
            "GET",
            &format!("/api/messages?channel_id={channel}"),
            Some(&to.token),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(!history.as_array().unwrap().is_empty());
    }
    // The group shows co-members' names and pictures, and nothing else of them.
    assert!(f.sees_member(b, c).await && f.sees_member(c, b).await);
    assert_eq!(f.profile(b, c).await, StatusCode::OK);
    assert_eq!(
        f.profile(d, c).await,
        StatusCode::FORBIDDEN,
        "a friend outside the group"
    );
    let boot = f.boot(b).await;
    assert!(
        boot["social"]["friends"]
            .as_array()
            .unwrap()
            .iter()
            .all(|id| *id != c.user.id),
        "sharing a group makes no friendship"
    );
    assert!(
        boot["presence"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["user_id"] != c.user.id),
        "no presence or game activity of a co-member who is not a friend"
    );
    // No direct message, call or search comes from sharing a group.
    let (status, _) = request(
        &f.app,
        "POST",
        "/api/messages",
        Some(&b.token),
        json!({"recipient_id":c.user.id,"content":CIPHERTEXT}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = request(
        &f.app,
        "POST",
        "/api/private-calls",
        Some(&b.token),
        json!({"peer_id":c.user.id}),
    )
    .await;
    assert!(status.is_client_error());
}

#[tokio::test]
async fn acceptance_rechecks_authority_friendship_and_blocks_and_stale_invitations_vanish() {
    let f = Friends::new();
    let p = f.people(&["alice", "bob", "cara", "dave"]);
    let (a, b, c, d) = (&p[0], &p[1], &p[2], &p[3]);
    for other in [b, c, d] {
        f.befriend(a, other).await;
    }
    let (_, made) = f.create(a, "Weekend", &[b]).await;
    let channel = made["channel_id"].as_str().unwrap().to_owned();
    let unfriend = |who: &Session, other: &Session| {
        let (app, path, token) = (
            f.app.clone(),
            format!("/api/friends/{}", other.user.id),
            who.token.clone(),
        );
        async move {
            request(&app, "DELETE", &path, Some(&token), Value::Null)
                .await
                .0
        }
    };
    let block = |who: &Session, other: &Session| {
        let (app, path, token) = (
            f.app.clone(),
            format!("/api/blocks/{}", other.user.id),
            who.token.clone(),
        );
        async move {
            request(&app, "POST", &path, Some(&token), Value::Null)
                .await
                .0
        }
    };
    let unblock = |who: &Session, other: &Session| {
        let (app, path, token) = (
            f.app.clone(),
            format!("/api/blocks/{}", other.user.id),
            who.token.clone(),
        );
        async move {
            request(&app, "DELETE", &path, Some(&token), Value::Null)
                .await
                .0
        }
    };
    // Ending the friendship ends the invitation; it cannot be accepted any more.
    assert_eq!(unfriend(a, b).await, StatusCode::OK);
    assert!(f.waiting(b).await.is_empty());
    assert_eq!(f.answer(b, &channel, "accept").await, StatusCode::GONE);
    assert_eq!(f.members(&channel).await, vec![a.user.id.clone()]);
    // A block, in either direction, ends it too.
    for blocker_is_invitee in [true, false] {
        f.befriend(a, b).await;
        assert_eq!(f.invite(a, &channel, b).await, StatusCode::OK);
        assert_eq!(f.waiting(b).await.len(), 1);
        let status = if blocker_is_invitee {
            block(b, a).await
        } else {
            block(a, b).await
        };
        assert_eq!(status, StatusCode::OK);
        assert!(f.waiting(b).await.is_empty());
        assert!(f.boot(a).await["group_invitations_sent"]
            .as_array()
            .unwrap()
            .is_empty());
        assert_eq!(f.answer(b, &channel, "accept").await, StatusCode::GONE);
        if blocker_is_invitee {
            unblock(b, a).await;
        } else {
            unblock(a, b).await;
        }
    }
    // An invitation into a group whose member the invitee blocks ends with the block.
    f.befriend(a, b).await;
    let channel = f.group(a, "Second", &[c]).await;
    assert_eq!(f.invite(a, &channel, b).await, StatusCode::OK);
    assert_eq!(block(b, c).await, StatusCode::OK);
    assert!(f.waiting(b).await.is_empty());
    assert_eq!(f.answer(b, &channel, "accept").await, StatusCode::GONE);
    unblock(b, c).await;
    // A member who blocked the invitee earlier: acceptance refuses, with the same
    // answer as any other stale invitation, and the invitation is gone.
    assert_eq!(block(c, b).await, StatusCode::OK);
    assert_eq!(f.invite(a, &channel, b).await, StatusCode::OK);
    assert_eq!(f.waiting(b).await.len(), 1);
    assert_eq!(f.answer(b, &channel, "accept").await, StatusCode::GONE);
    assert!(!f.members(&channel).await.contains(&b.user.id));
    assert!(f.waiting(b).await.is_empty());
    // The owner leaves: invitations they issued do not pass to the next owner.
    unblock(c, b).await;
    assert_eq!(f.invite(a, &channel, d).await, StatusCode::OK);
    assert_eq!(f.waiting(d).await.len(), 1);
    let (left, _) = request(
        &f.app,
        "DELETE",
        &format!("/api/conversations/{channel}"),
        Some(&a.token),
        Value::Null,
    )
    .await;
    assert_eq!(left, StatusCode::NO_CONTENT);
    assert_eq!(f.owner(&channel), c.user.id);
    assert!(f.waiting(d).await.is_empty());
    assert_eq!(f.answer(d, &channel, "accept").await, StatusCode::GONE);
    // The new owner is not Dave's friend, so cannot invite him either.
    assert_eq!(f.invite(c, &channel, d).await, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn only_the_owner_invites_only_friends_and_old_invitations_lapse() {
    let f = Friends::new();
    let p = f.people(&["alice", "bob", "cara", "dave", "erin"]);
    let (a, b, c, d, e) = (&p[0], &p[1], &p[2], &p[3], &p[4]);
    for other in [b, c, d] {
        f.befriend(a, other).await;
    }
    f.befriend(b, e).await;
    let channel = f.group(a, "Weekend", &[b]).await;
    // A member who is not the owner cannot invite, even a friend of their own.
    assert_eq!(f.invite(b, &channel, e).await, StatusCode::FORBIDDEN);
    // The owner cannot invite a stranger, themselves, or someone already inside.
    assert_eq!(f.invite(a, &channel, e).await, StatusCode::FORBIDDEN);
    assert_eq!(f.invite(a, &channel, a).await, StatusCode::BAD_REQUEST);
    assert_eq!(f.invite(a, &channel, b).await, StatusCode::BAD_REQUEST);
    // Neither a member nor a stranger can revoke an invitation.
    assert_eq!(f.invite(a, &channel, c).await, StatusCode::OK);
    for who in [b, d] {
        let (status, _) = request(
            &f.app,
            "DELETE",
            &format!("/api/conversations/{channel}/invitations/{}", c.user.id),
            Some(&who.token),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
    assert_eq!(f.waiting(c).await.len(), 1);
    // Inviting again changes nothing; revoking removes it; declining an absent one is harmless.
    assert_eq!(f.invite(a, &channel, c).await, StatusCode::OK);
    assert_eq!(f.waiting(c).await.len(), 1);
    let (status, _) = request(
        &f.app,
        "DELETE",
        &format!("/api/conversations/{channel}/invitations/{}", c.user.id),
        Some(&a.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(f.waiting(c).await.is_empty());
    assert_eq!(f.answer(c, &channel, "accept").await, StatusCode::GONE);
    assert_eq!(
        f.answer(c, &channel, "decline").await,
        StatusCode::NO_CONTENT
    );
    // A declined invitation cannot be accepted afterwards.
    assert_eq!(f.invite(a, &channel, c).await, StatusCode::OK);
    assert_eq!(
        f.answer(c, &channel, "decline").await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(f.answer(c, &channel, "accept").await, StatusCode::GONE);
    // An invitation older than a week is ignored and cannot be accepted.
    assert_eq!(f.invite(a, &channel, d).await, StatusCode::OK);
    lock(&f.state.0.db)
        .execute(
            "UPDATE group_invitations SET created=created-?1",
            [(relay_protocol::PEOPLE_GROUP_INVITE_DAYS + 1) * 86400],
        )
        .unwrap();
    assert!(f.waiting(d).await.is_empty());
    assert_eq!(f.answer(d, &channel, "accept").await, StatusCode::GONE);
    assert!(!f.members(&channel).await.contains(&d.user.id));
}

#[tokio::test]
async fn a_group_holds_eight_counting_invitations_and_each_person_has_a_waiting_limit() {
    let f = Friends::new();
    let names = ["p0", "p1", "p2", "p3", "p4", "p5", "p6", "p7", "p8", "p9"];
    let p = f.people(&names);
    for other in &p[1..] {
        f.befriend(&p[0], other).await;
    }
    let seven: Vec<&Session> = p[1..8].iter().collect();
    let eight: Vec<&Session> = p[1..9].iter().collect();
    assert_eq!(
        f.create(&p[0], "Nine", &eight).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        f.create(&p[0], "None", &[]).await.0,
        StatusCode::BAD_REQUEST
    );
    let (status, made) = f.create(&p[0], "Eight", &seven).await;
    assert_eq!(status, StatusCode::OK, "{made}");
    let channel = made["channel_id"].as_str().unwrap().to_owned();
    // Seven waiting invitations plus the owner fill the group.
    assert_eq!(
        f.invite(&p[0], &channel, &p[8]).await,
        StatusCode::BAD_REQUEST
    );
    // A decline frees the place; an acceptance keeps it.
    assert_eq!(
        f.answer(&p[1], &channel, "decline").await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(f.invite(&p[0], &channel, &p[8]).await, StatusCode::OK);
    assert_eq!(f.answer(&p[2], &channel, "accept").await, StatusCode::OK);
    assert_eq!(
        f.invite(&p[0], &channel, &p[1]).await,
        StatusCode::BAD_REQUEST
    );
    for who in &p[3..=8] {
        assert_eq!(f.answer(who, &channel, "accept").await, StatusCode::OK);
    }
    assert_eq!(f.members(&channel).await.len(), 8);
    assert_eq!(
        f.invite(&p[0], &channel, &p[9]).await,
        StatusCode::BAD_REQUEST
    );
    // A person with fifty invitations waiting cannot be invited again.
    let other = f.group(&p[0], "Other", &[&p[1]]).await;
    {
        let db = lock(&f.state.0.db);
        db.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
        for index in 0..relay_protocol::PEOPLE_GROUP_PENDING_PER_PERSON {
            db.execute(
                "INSERT INTO group_invitations(channel_id,invitee,inviter,created) VALUES(?1,?2,?3,?4)",
                params![format!("synthetic-{index}"), p[9].user.id, p[0].user.id, now()],
            )
            .unwrap();
        }
        db.execute_batch("PRAGMA foreign_keys=ON").unwrap();
    }
    assert_eq!(
        f.invite(&p[0], &other, &p[9]).await,
        StatusCode::BAD_REQUEST
    );
    // Creating another group uses the same admission budget, and must not
    // leave a partial group/invitation for the other invited friend.
    let counts = || {
        let db = lock(&f.state.0.db);
        (
            db.query_row("SELECT COUNT(*) FROM conversations", [], |r| {
                r.get::<_, u32>(0)
            })
            .unwrap(),
            db.query_row(
                "SELECT COUNT(*) FROM group_invitations WHERE invitee=?1",
                [&p[8].user.id],
                |r| r.get::<_, u32>(0),
            )
            .unwrap(),
        )
    };
    let before = counts();
    assert_eq!(
        f.create(&p[0], "Must not partially create", &[&p[8], &p[9]])
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(counts(), before);
    {
        let db = lock(&f.state.0.db);
        db.execute(
            "UPDATE group_invitations SET created=?1 WHERE channel_id='synthetic-0'",
            [now().saturating_sub(8 * 86400)],
        )
        .unwrap();
    }
    assert_eq!(
        f.create(&p[0], "Expired reservation gives room", &[&p[9]])
            .await
            .0,
        StatusCode::OK
    );
    let db = lock(&f.state.0.db);
    let pending: u32 = db
        .query_row(
            "SELECT COUNT(*) FROM group_invitations WHERE invitee=?1",
            [&p[9].user.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(pending, 50);
}

#[tokio::test]
async fn removal_leaving_and_blocks_end_access_while_unfriending_changes_no_membership() {
    let f = Friends::new();
    let p = f.people(&["alice", "bob", "cara", "dave"]);
    let (a, b, c, d) = (&p[0], &p[1], &p[2], &p[3]);
    for other in [b, c, d] {
        f.befriend(a, other).await;
    }
    let channel = f.group(a, "Weekend", &[b, c]).await;
    // A file shared in the group.
    let (status, uploaded) = raw(
        &f.app,
        "POST",
        "/api/uploads?name=note.bin&mime=application%2Foctet-stream",
        &a.token,
        b"bytes only the group may fetch",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let attachment: Attachment = serde_json::from_slice(&uploaded).unwrap();
    let (status, _) = request(
        &f.app,
        "POST",
        "/api/messages",
        Some(&a.token),
        json!({"channel_id":channel,"content":CIPHERTEXT,"attachment_ids":[attachment.id]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let download = |who: &Session| {
        let (app, url, token) = (f.app.clone(), attachment.url.clone(), who.token.clone());
        async move { raw(&app, "GET", &url, &token, b"").await.0 }
    };
    for member in [a, b, c] {
        assert_eq!(download(member).await, StatusCode::OK);
    }
    assert_eq!(
        download(d).await,
        StatusCode::FORBIDDEN,
        "a friend outside the group"
    );
    // Ending a friendship removes nobody; two members who never were friends stay.
    for (who, other) in [(b, c), (a, b)] {
        let (status, _) = request(
            &f.app,
            "DELETE",
            &format!("/api/friends/{}", other.user.id),
            Some(&who.token),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    assert_eq!(f.members(&channel).await.len(), 3);
    assert!(f.can(&channel, a, SEND) && f.can(&channel, b, SEND) && f.can(&channel, c, SEND));
    // Taking someone out ends their access to messages, files and the others' pictures.
    let (status, _) = request(
        &f.app,
        "PATCH",
        &format!("/api/conversations/{channel}"),
        Some(&a.token),
        json!({"member_ids":[a.user.id, c.user.id]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!f.can(&channel, b, READ));
    assert_eq!(download(b).await, StatusCode::FORBIDDEN);
    assert_eq!(download(c).await, StatusCode::OK);
    assert!(!f.sees_member(b, c).await);
    assert_eq!(f.profile(b, c).await, StatusCode::FORBIDDEN);
    // Someone who blocks a co-member leaves the group at once; the rest carry on.
    let channel = f.group(a, "Again", &[c, d]).await;
    let (status, _) = request(
        &f.app,
        "POST",
        &format!("/api/blocks/{}", d.user.id),
        Some(&c.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!f.can(&channel, c, READ));
    assert!(f.can(&channel, a, SEND) && f.can(&channel, d, SEND));
    assert_eq!(f.owner(&channel), a.user.id);
    // A member who blocks the owner cannot take the group.
    let (status, _) = request(
        &f.app,
        "POST",
        &format!("/api/blocks/{}", a.user.id),
        Some(&d.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(f.owner(&channel), a.user.id);
    assert_eq!(f.members(&channel).await, vec![a.user.id.clone()]);
    // Leaving ends access the same way.
    let channel = f.group(a, "Last", &[c]).await;
    let (status, _) = request(
        &f.app,
        "DELETE",
        &format!("/api/conversations/{channel}"),
        Some(&c.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(!f.can(&channel, c, READ));
}

#[tokio::test]
async fn people_are_added_only_by_invitation_and_only_the_known_routes_are_open() {
    let f = Friends::new();
    let p = f.people(&["alice", "bob", "cara"]);
    let (a, b, c) = (&p[0], &p[1], &p[2]);
    f.befriend(a, b).await;
    f.befriend(a, c).await;
    let channel = f.group(a, "Weekend", &[b]).await;
    // Editing the member list can only remove: a friend is not added by naming them.
    for body in [
        json!({"member_ids":[a.user.id, b.user.id, c.user.id]}),
        json!({"member_ids":[b.user.id]}),
        json!({"member_ids":[a.user.id, a.user.id, b.user.id]}),
    ] {
        let (status, _) = request(
            &f.app,
            "PATCH",
            &format!("/api/conversations/{channel}"),
            Some(&a.token),
            body,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
    assert_eq!(f.members(&channel).await.len(), 2);
    for (method, path) in [
        ("GET", "/api/conversations".to_string()),
        ("PATCH", "/api/conversations".to_string()),
        ("GET", format!("/api/conversations/{channel}")),
        ("POST", format!("/api/conversations/{channel}")),
        ("PUT", format!("/api/conversations/{channel}/stage")),
        ("GET", format!("/api/conversations/{channel}/invitations")),
        (
            "DELETE",
            format!("/api/conversations/{channel}/invitations"),
        ),
        (
            "POST",
            format!("/api/conversations/{channel}/invitations/{}", c.user.id),
        ),
        (
            "PATCH",
            format!("/api/conversations/{channel}/invitations/{}", c.user.id),
        ),
        ("GET", format!("/api/group-invitations/{channel}/accept")),
        ("PUT", format!("/api/group-invitations/{channel}/accept")),
        (
            "DELETE",
            format!("/api/group-invitations/{channel}/decline"),
        ),
        ("POST", format!("/api/group-invitations/{channel}")),
        (
            "POST",
            format!("/api/group-invitations/{channel}/accept/more"),
        ),
        ("POST", "/api/scheduled-events".to_string()),
        ("PUT", format!("/api/channels/{channel}/permissions")),
        ("DELETE", format!("/api/channels/{channel}")),
    ] {
        let (status, _) = request(&f.app, method, &path, Some(&a.token), json!({})).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}");
    }
}

#[tokio::test]
async fn only_members_read_and_send_and_only_ciphertext_is_accepted() {
    let f = Friends::new();
    let p = f.people(&["alice", "bob", "cara", "dave"]);
    let (a, b, c, d) = (&p[0], &p[1], &p[2], &p[3]);
    for other in [b, c, d] {
        f.befriend(a, other).await;
    }
    let channel = f.group(a, "Trio", &[b, c]).await;
    for member in [a, b, c] {
        let (status, sent) = request(
            &f.app,
            "POST",
            "/api/messages",
            Some(&member.token),
            json!({"channel_id":channel,"content":CIPHERTEXT}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{sent}");
    }
    let (status, _) = request(
        &f.app,
        "POST",
        "/api/messages",
        Some(&a.token),
        json!({"channel_id":channel,"content":"hello"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // A friend of the owner who was never invited can neither send nor read.
    let (status, _) = request(
        &f.app,
        "POST",
        "/api/messages",
        Some(&d.token),
        json!({"channel_id":channel,"content":CIPHERTEXT}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = request(
        &f.app,
        "GET",
        &format!("/api/messages?channel_id={channel}"),
        Some(&d.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, history) = request(
        &f.app,
        "GET",
        &format!("/api/messages?channel_id={channel}"),
        Some(&b.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(history.as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn the_owner_leaving_hands_the_group_on_and_the_last_one_out_closes_it() {
    let f = Friends::new();
    let p = f.people(&["alice", "bob", "cara"]);
    let (a, b, c) = (&p[0], &p[1], &p[2]);
    f.befriend(a, b).await;
    f.befriend(a, c).await;
    let channel = f.group(a, "Trio", &[b, c]).await;
    let leave = |who: &Session| {
        let (app, path, token) = (
            f.app.clone(),
            format!("/api/conversations/{channel}"),
            who.token.clone(),
        );
        async move {
            request(&app, "DELETE", &path, Some(&token), Value::Null)
                .await
                .0
        }
    };
    let edit = |who: &Session, body: Value| {
        let (app, path, token) = (
            f.app.clone(),
            format!("/api/conversations/{channel}"),
            who.token.clone(),
        );
        async move { request(&app, "PATCH", &path, Some(&token), body).await.0 }
    };
    // Bob cannot rename it; the owner can.
    assert_eq!(edit(b, json!({"name":"Mine"})).await, StatusCode::FORBIDDEN);
    assert_eq!(edit(a, json!({"name":"Weekend"})).await, StatusCode::OK);
    assert_eq!(leave(a).await, StatusCode::NO_CONTENT);
    let next = f.owner(&channel);
    assert!(next == b.user.id || next == c.user.id);
    assert_eq!(leave(b).await, StatusCode::NO_CONTENT);
    assert_eq!(leave(c).await, StatusCode::NO_CONTENT);
    assert!(conversations::info(&lock(&f.state.0.db), &channel)
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn a_pairs_call_room_keeps_the_friend_pair_rule_and_is_not_a_group() {
    let f = Friends::new();
    let p = f.people(&["alice", "bob", "cara"]);
    let (a, b, c) = (&p[0], &p[1], &p[2]);
    f.befriend(a, b).await;
    f.befriend(a, c).await;
    let (status, call) = request(
        &f.app,
        "POST",
        "/api/private-calls",
        Some(&a.token),
        json!({"peer_id":b.user.id}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{call}");
    let room = call["channel_id"].as_str().unwrap();
    assert!(f.can(room, a, JOIN) && f.can(room, b, JOIN));
    // The room is the pair's alone: nobody can be invited into it, and
    // unfriending closes it as before.
    let parent = conversations::info(&lock(&f.state.0.db), room)
        .unwrap()
        .unwrap()
        .parent_id
        .unwrap();
    assert_eq!(f.invite(a, &parent, c).await, StatusCode::NOT_FOUND);
    let (status, _) = request(
        &f.app,
        "DELETE",
        &format!("/api/friends/{}", b.user.id),
        Some(&a.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!f.can(room, a, JOIN) && !f.can(room, b, JOIN));
    // A pair's room never makes the two co-members of a group.
    assert!(!f.sees_member(b, a).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nonfriend_group_events_reach_the_live_socket_without_global_presence_or_dm_access() {
    use futures_util::StreamExt;
    use tokio_tungstenite::{
        tungstenite::{client::IntoClientRequest, Message},
        MaybeTlsStream, WebSocketStream,
    };
    async fn event(socket: &mut WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>) -> Event {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                match socket.next().await {
                    Some(Ok(Message::Text(raw))) => return serde_json::from_str(&raw).unwrap(),
                    Some(Ok(_)) => {}
                    _ => panic!("event socket closed"),
                }
            }
        })
        .await
        .expect("expected scoped event")
    }
    let f = Friends::new();
    let p = f.people(&["alice", "bob", "cara", "dave"]);
    let (a, b, c, d) = (&p[0], &p[1], &p[2], &p[3]);
    for friend in [b, c, d] {
        f.befriend(a, friend).await;
    }
    let channel = f.group(a, "Together", &[b, c]).await;
    let elsewhere = f.group(a, "Elsewhere", &[c, d]).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = f.app.clone();
    let stopped = f.state.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move { stopped.shutdown_requested().await })
        .await
        .unwrap();
    });
    let mut request = format!("ws://{address}/api/events")
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        header::AUTHORIZATION,
        format!("Bearer {}", b.token).parse().unwrap(),
    );
    let mut socket = tokio_tungstenite::connect_async(request).await.unwrap().0;
    // The upgrade's first heartbeat proves it subscribed before the message.
    tokio::time::timeout(Duration::from_secs(3), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let (status, _) = request_group_message(&f, c, &channel).await;
    assert_eq!(status, StatusCode::OK);
    let received = event(&mut socket).await;
    assert_eq!(received.kind, "messages");
    assert_eq!(received.channel_id.as_deref(), Some(channel.as_str()));
    assert_eq!(received.user_id.as_deref(), Some(c.user.id.as_str()));
    let (status, _) = crate::tests::request(
        &f.app,
        "POST",
        "/api/typing",
        Some(&c.token),
        json!({"channel_id":channel,"typing":true}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(event(&mut socket).await.kind, "typing");
    let (dm_typing, _) = crate::tests::request(
        &f.app,
        "POST",
        "/api/typing",
        Some(&c.token),
        json!({"peer_id":b.user.id,"typing":true}),
    )
    .await;
    assert_eq!(dm_typing, StatusCode::FORBIDDEN);
    // A coarse group status change is visible, but changing only a private
    // note (including while invisible) must not signal private activity.
    f.set_status(c, "dnd", "private status one").await;
    assert_eq!(
        event(&mut socket).await.kind,
        relay_protocol::GROUP_PRESENCE_EVENT
    );
    f.set_status(c, "dnd", "private status two").await;
    f.state
        .notify("channels", None, None, Some(vec![b.user.id.clone()]));
    assert_eq!(event(&mut socket).await.kind, "channels");
    f.set_status(c, "invisible", "hidden status one").await;
    assert_eq!(
        event(&mut socket).await.kind,
        relay_protocol::GROUP_PRESENCE_EVENT
    );
    f.set_status(c, "invisible", "hidden status two").await;
    // Events are ordered: the next permitted sentinel must be first, so no
    // sleep-based assertion or timing guess is needed for forbidden events.
    f.state
        .notify("presence", None, Some(c.user.id.clone()), None);
    f.state
        .notify("messages", Some(elsewhere), Some(c.user.id.clone()), None);
    f.state.notify(
        "messages",
        None,
        Some(c.user.id.clone()),
        Some(vec![b.user.id.clone(), c.user.id.clone()]),
    );
    f.state
        .notify("channels", None, None, Some(vec![b.user.id.clone()]));
    assert_eq!(event(&mut socket).await.kind, "channels");
    let (status, _) = crate::tests::request(
        &f.app,
        "DELETE",
        &format!("/api/conversations/{channel}"),
        Some(&b.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(event(&mut socket).await.kind, "channels");
    f.state.notify(
        relay_protocol::GROUP_PRESENCE_EVENT,
        Some(channel.clone()),
        Some(c.user.id.clone()),
        Some(vec![b.user.id.clone()]),
    );
    f.state
        .notify("messages", Some(channel), Some(c.user.id.clone()), None);
    f.state
        .notify("channels", None, None, Some(vec![b.user.id.clone()]));
    assert_eq!(event(&mut socket).await.kind, "channels");
    socket.close(None).await.unwrap();
    f.state.request_shutdown();
    server.await.unwrap();
}
async fn request_group_message(f: &Friends, from: &Session, channel: &str) -> (StatusCode, Value) {
    request(
        &f.app,
        "POST",
        "/api/messages",
        Some(&from.token),
        json!({"channel_id":channel,"content":CIPHERTEXT}),
    )
    .await
}

impl Friends {
    async fn set_status(&self, who: &Session, status: &str, text: &str) {
        let (code, _) = request(
            &self.app,
            "PUT",
            "/api/presence",
            Some(&who.token),
            json!({"status":status,"text":text}),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
    }
    /// What `viewer` is told about the people in their groups: (channel, person, state).
    async fn group_states(&self, viewer: &Session) -> Vec<(String, String, String)> {
        self.boot(viewer).await["group_presence"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                (
                    row["channel_id"].as_str().unwrap().to_owned(),
                    row["user_id"].as_str().unwrap().to_owned(),
                    row["status"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    }
    async fn state_of(&self, viewer: &Session, channel: &str, person: &Session) -> Option<String> {
        self.group_states(viewer)
            .await
            .into_iter()
            .find(|(c, u, _)| c == channel && *u == person.user.id)
            .map(|(_, _, s)| s)
    }
}

#[tokio::test]
async fn accepted_co_members_see_coarse_state_in_their_group_and_no_one_else_does() {
    let f = Friends::new();
    let p = f.people(&["alice", "bob", "cara", "dave", "eve"]);
    let (a, b, c, d, e) = (&p[0], &p[1], &p[2], &p[3], &p[4]);
    for other in [b, c, d, e] {
        f.befriend(a, other).await;
    }
    let channel = f.group(a, "Weekend", &[b, c]).await;
    // Eve is only invited; Dave is Alice's friend but not in the group.
    assert_eq!(f.invite(a, &channel, e).await, StatusCode::OK);
    f.set_status(c, "dnd", "private note about the weekend")
        .await;
    f.set_status(a, "away", "").await;

    // Bob and Cara are not friends, yet Bob sees Cara's coarse state in the group.
    let boot = f.boot(b).await;
    assert!(boot["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .any(|cap| cap == relay_protocol::PEOPLE_GROUP_PRESENCE_CAPABILITY));
    assert_eq!(f.state_of(b, &channel, c).await.as_deref(), Some("dnd"));
    assert_eq!(f.state_of(b, &channel, a).await.as_deref(), Some("idle"));
    // Never himself, never someone outside the group.
    assert_eq!(f.state_of(b, &channel, b).await, None);
    assert!(f
        .group_states(b)
        .await
        .iter()
        .all(|(g, u, _)| *g == channel && *u != d.user.id && *u != e.user.id));
    // The rows hold exactly channel, person and a coarse state: no text, time or activity.
    let rows = &boot["group_presence"];
    assert!(rows.as_array().unwrap().iter().all(|row| {
        row.as_object().unwrap().len() == 3
            && ["online", "idle", "dnd", "offline"].contains(&row["status"].as_str().unwrap())
    }));
    assert!(!rows.to_string().contains("private note"));
    // Global presence stays friends and self only: Bob still does not see Cara there.
    assert!(boot["presence"]
        .as_array()
        .unwrap()
        .iter()
        .all(|row| row["user_id"] != c.user.id));
    // A pending invitee and an outsider get nothing.
    assert!(f.group_states(e).await.is_empty());
    assert!(f.group_states(d).await.is_empty());
}

#[tokio::test]
async fn hidden_stale_removed_and_blocked_people_are_not_shown_as_present() {
    let f = Friends::new();
    let p = f.people(&["alice", "bob", "cara"]);
    let (a, b, c) = (&p[0], &p[1], &p[2]);
    f.befriend(a, b).await;
    f.befriend(a, c).await;
    let channel = f.group(a, "Trio", &[b, c]).await;
    f.set_status(c, "online", "").await;
    assert_eq!(f.state_of(b, &channel, c).await.as_deref(), Some("online"));
    // Invisible reads as offline.
    f.set_status(c, "invisible", "").await;
    assert_eq!(f.state_of(b, &channel, c).await.as_deref(), Some("offline"));
    // A heartbeat that has gone stale reads as offline whatever was chosen.
    f.set_status(c, "online", "").await;
    lock(&f.state.0.db)
        .execute(
            "UPDATE user_presence SET last_seen=last_seen-1000 WHERE user_id=?1",
            [&c.user.id],
        )
        .unwrap();
    assert_eq!(f.state_of(b, &channel, c).await.as_deref(), Some("offline"));
    // Cara blocks Bob: Cara leaves the shared group, so neither lists the other there.
    f.set_status(c, "online", "").await;
    let (status, _) = request(
        &f.app,
        "POST",
        &format!("/api/blocks/{}", b.user.id),
        Some(&c.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(f.state_of(b, &channel, c).await, None);
    assert!(f.group_states(c).await.is_empty());
    // Alice takes Bob out: he no longer receives anything for the group.
    let (status, _) = request(
        &f.app,
        "PATCH",
        &format!("/api/conversations/{channel}"),
        Some(&a.token),
        json!({"member_ids":[a.user.id]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(f.group_states(b).await.is_empty());
}

#[tokio::test]
async fn a_pairs_call_room_has_no_group_presence_and_the_event_goes_only_to_current_members() {
    let f = Friends::new();
    let p = f.people(&["alice", "bob", "cara", "dave"]);
    let (a, b, c, d) = (&p[0], &p[1], &p[2], &p[3]);
    for other in [b, c, d] {
        f.befriend(a, other).await;
    }
    // The pair's call room is not a group.
    let (status, _) = request(
        &f.app,
        "POST",
        "/api/private-calls",
        Some(&a.token),
        json!({"peer_id":d.user.id}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(f.group_states(a).await.is_empty());
    assert!(f.group_states(d).await.is_empty());
    // A change of state tells exactly the group's current members, by channel and person only.
    let channel = f.group(a, "Trio", &[b, c]).await;
    let mut events = f.state.0.events.subscribe();
    f.set_status(c, "away", "back soon").await;
    let mut seen = vec![];
    while let Ok(notice) = events.try_recv() {
        if notice.event.kind == relay_protocol::GROUP_PRESENCE_EVENT {
            seen.push(notice);
        }
    }
    assert_eq!(seen.len(), 1);
    let event = &seen[0].event;
    assert_eq!(event.channel_id.as_deref(), Some(channel.as_str()));
    assert_eq!(event.user_id.as_deref(), Some(c.user.id.as_str()));
    assert!(event.typing.is_none() && event.recipient_id.is_none());
    let audience = sorted(seen[0].audience.clone().unwrap());
    assert_eq!(
        audience,
        sorted(vec![
            a.user.id.clone(),
            b.user.id.clone(),
            c.user.id.clone()
        ])
    );
}
