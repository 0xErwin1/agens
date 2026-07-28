//! Provider-resolution tests that build their fixtures through the CLI's test
//! support, so they live here rather than inside `agens-session`.

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::test_support::tui_session_directory;
    use agens_session::provider::*;

    #[test]
    fn tui_provider_availability_uses_complete_current_credentials_without_exposing_them() {
        let temporary = tui_session_directory("provider-status");
        let credentials = temporary.join("auth.json");
        std::fs::write(
            &credentials,
            r#"{"openai-chatgpt":{"access_token":"access","refresh_token":"refresh","account_id":"account","expires_at":"2099-01-01T00:00:00Z"}}"#,
        )
        .unwrap();
        let resolver = CredentialResolver::with_environment(BTreeMap::new());

        let statuses =
            ProviderKind::ALL.map(|provider| resolver.status(&credentials, provider).label());
        assert_eq!(statuses, ["ready", "credential required"]);
        std::fs::remove_dir_all(temporary).unwrap();
    }
}
