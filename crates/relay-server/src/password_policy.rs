//! Offline checks for NEW passwords only. Existing passwords still authenticate.
use super::*;
use std::sync::OnceLock;

pub(super) fn validate(username: &str, password: &str) -> ApiResult<()> {
    if !(10..=256).contains(&password.len()) {
        return Err(bad("Password must be 10–256 characters"));
    }
    let comparable = password.trim().to_lowercase();
    if comparable == username.trim().to_lowercase() {
        return Err(bad("Choose a password different from your username"));
    }
    static COMMON: OnceLock<Vec<&'static str>> = OnceLock::new();
    let common = COMMON.get_or_init(|| {
        include_str!("../../../assets/security/common-passwords.txt")
            .lines()
            .collect()
    });
    if common.binary_search(&comparable.as_str()).is_ok() {
        return Err(bad(
            "That password is commonly used. Choose a longer, unique passphrase",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::request;

    #[test]
    fn list_is_sorted_unique_and_new_passwords_refuse_common_and_username_variants() {
        let list: Vec<_> = include_str!("../../../assets/security/common-passwords.txt")
            .lines()
            .collect();
        assert!(list.len() >= 10_000);
        assert!(list.windows(2).all(|pair| pair[0] < pair[1]));
        for password in [
            "password123456",
            "Password123456",
            "  PASSWORD123456 ",
            "abcdefghij",
            "1234567890",
            "ExamplePerson",
        ] {
            assert!(validate("ExamplePerson", password).is_err());
        }
        assert!(validate("ExamplePerson", "river lantern sandal orchard 39").is_ok());
    }

    #[tokio::test]
    async fn weak_passwords_cannot_be_created_but_existing_weak_passwords_still_login_and_can_change(
    ) {
        let data = tempfile::tempdir().unwrap();
        let state = AppState::open(data.path(), "password policy", 0).unwrap();
        let app = router(state.clone());
        assert_eq!(
            request(
                &app,
                "POST",
                "/api/register",
                None,
                serde_json::json!({"username":"OwnerName","password":"password123456"})
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        assert!(!state.has_users().unwrap());
        state
            .initialize_admin("OwnerName", "river lantern sandal orchard 39")
            .unwrap();
        // A legacy database may contain a password now blocked for creation.
        let old = "password123456";
        let hash = Argon2::default()
            .hash_password(old.as_bytes(), &SaltString::generate(&mut OsRng))
            .unwrap()
            .to_string();
        lock(&state.0.db)
            .execute("UPDATE users SET password_hash=?1", [hash])
            .unwrap();
        let (status, session) = request(
            &app,
            "POST",
            "/api/login",
            None,
            serde_json::json!({"username":"OwnerName","password":old}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let token = session["token"].as_str().unwrap();
        assert_eq!(
            request(
                &app,
                "PATCH",
                "/api/password",
                Some(token),
                serde_json::json!({"current_password":old,"new_password":"1234567890"})
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(request(&app, "PATCH", "/api/password", Some(token),
            serde_json::json!({"current_password":old,"new_password":"meadow apple balloon compass 72"})).await.0,
            StatusCode::NO_CONTENT);
    }
}
