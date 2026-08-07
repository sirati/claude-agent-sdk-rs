//! Pre-flight validation for `ClaudeAgentOptions.session_store` combinations.

use crate::errors::{ClaudeError, Result};
use crate::types::config::ClaudeAgentOptions;

/// Validate `session_store`-related option combinations.
///
/// Called before subprocess spawn so misconfiguration fails fast instead of
/// surfacing as a confusing runtime error mid-session.
pub fn validate_session_store_options(options: &ClaudeAgentOptions) -> Result<()> {
    let Some(store) = &options.session_store else {
        return Ok(());
    };

    // When resume is explicitly set, list_sessions() is provably never
    // called (resume wins over continue), so a minimal store is fine.
    if options.continue_conversation
        && options.resume.is_none()
        && !store.supports_list_sessions()
    {
        return Err(ClaudeError::InvalidConfig(
            "continue_conversation with session_store requires the store to \
             implement list_sessions()"
                .to_string(),
        ));
    }

    if options.enable_file_checkpointing {
        return Err(ClaudeError::InvalidConfig(
            "session_store cannot be combined with enable_file_checkpointing \
             (checkpoints are local-disk only and would diverge from the \
             mirrored transcript)"
                .to_string(),
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_session_store_options;
    use crate::session::{SessionKey, SessionStore, SessionStoreEntry, SessionStoreError};
    use crate::types::config::ClaudeAgentOptions;
    use async_trait::async_trait;
    use std::sync::Arc;

    struct MinimalStore;

    #[async_trait]
    impl SessionStore for MinimalStore {
        async fn append(
            &self,
            _key: &SessionKey,
            _entries: Vec<SessionStoreEntry>,
        ) -> std::result::Result<(), SessionStoreError> {
            Ok(())
        }

        async fn load(
            &self,
            _key: &SessionKey,
        ) -> std::result::Result<Option<Vec<SessionStoreEntry>>, SessionStoreError> {
            Ok(None)
        }
    }

    #[test]
    fn no_store_is_always_valid() {
        let options = ClaudeAgentOptions::builder()
            .continue_conversation(true)
            .enable_file_checkpointing(true)
            .build();
        assert!(validate_session_store_options(&options).is_ok());
    }

    #[test]
    fn store_implementing_list_sessions_passes() {
        let options = ClaudeAgentOptions::builder()
            .session_store(Arc::new(crate::session::InMemorySessionStore::new()) as Arc<dyn SessionStore>)
            .continue_conversation(true)
            .build();
        assert!(validate_session_store_options(&options).is_ok());
    }

    #[test]
    fn continue_conversation_requires_list_sessions() {
        let options = ClaudeAgentOptions::builder()
            .session_store(Arc::new(MinimalStore) as Arc<dyn SessionStore>)
            .continue_conversation(true)
            .build();
        let err = validate_session_store_options(&options).unwrap_err();
        assert!(err.to_string().contains("list_sessions"));
    }

    #[test]
    fn continue_with_explicit_resume_does_not_require_list_sessions() {
        // Parity with upstream: when resume is explicitly set, continue=true
        // should not require list_sessions() — it is provably never called
        // because resume wins.
        let options = ClaudeAgentOptions::builder()
            .session_store(Arc::new(MinimalStore) as Arc<dyn SessionStore>)
            .continue_conversation(true)
            .resume("00000000-0000-4000-8000-000000000000")
            .build();
        assert!(validate_session_store_options(&options).is_ok());
    }

    #[test]
    fn rejects_file_checkpointing_combo() {
        let options = ClaudeAgentOptions::builder()
            .session_store(Arc::new(crate::session::InMemorySessionStore::new()) as Arc<dyn SessionStore>)
            .enable_file_checkpointing(true)
            .build();
        let err = validate_session_store_options(&options).unwrap_err();
        assert!(err.to_string().contains("enable_file_checkpointing"));
    }
}
