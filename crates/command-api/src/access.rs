//! What an adapter lets its client run.
//!
//! Adapters that serve less trusted clients (agents over MCP) authorize
//! every decoded command against an [`AccessPolicy`] before the engine sees
//! it. The policy is chosen by the human who starts the adapter.

use serde_json::json;

use crate::{Command, CommandAccess, CommandError, CommandHandler, CommandOutput, ErrorCode};

/// Allows commands up to an access level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccessPolicy {
    max: CommandAccess,
}

impl AccessPolicy {
    /// Only commands that inspect state.
    pub const READ_ONLY: Self = Self {
        max: CommandAccess::ReadOnly,
    };

    pub const fn allowing(max: CommandAccess) -> Self {
        Self { max }
    }

    pub fn max(&self) -> CommandAccess {
        self.max
    }

    pub fn allows(&self, access: CommandAccess) -> bool {
        access <= self.max
    }

    /// `permission_denied` unless `command` needs at most the allowed level.
    pub fn authorize(&self, command: &Command) -> Result<(), CommandError> {
        let required = command.required_access();
        if self.allows(required) {
            return Ok(());
        }
        let name = command.descriptor().name;
        Err(CommandError::new(
            ErrorCode::PermissionDenied,
            format!(
                "`{name}` needs {} access, but this adapter allows only {}; the person running \
                 it can allow more",
                label(required),
                label(self.max)
            ),
        )
        .with_details(json!({
            "command": name,
            "required": required,
            "allowed": self.max,
        })))
    }
}

/// A handler that authorizes every command before passing it on, for
/// adapters that take typed commands (e.g. the JSON-lines server).
pub struct Gated<H> {
    inner: H,
    policy: AccessPolicy,
}

impl<H: CommandHandler> Gated<H> {
    pub fn new(inner: H, policy: AccessPolicy) -> Self {
        Self { inner, policy }
    }
}

impl<H: CommandHandler> CommandHandler for Gated<H> {
    fn handle(&mut self, command: Command) -> Result<CommandOutput, CommandError> {
        self.policy.authorize(&command)?;
        self.inner.handle(command)
    }
}

/// Human-readable name of an access level.
pub fn label(access: CommandAccess) -> &'static str {
    match access {
        CommandAccess::ReadOnly => "read-only",
        CommandAccess::Control => "control",
        CommandAccess::Sensitive => "sensitive",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo(usize);

    impl CommandHandler for Echo {
        fn handle(&mut self, _command: Command) -> Result<CommandOutput, CommandError> {
            self.0 += 1;
            Ok(CommandOutput::Commands(crate::CommandList::all()))
        }
    }

    #[test]
    fn gated_handlers_never_see_denied_commands() {
        let mut gated = Gated::new(Echo(0), AccessPolicy::READ_ONLY);
        assert!(gated.handle(Command::ListCommands).is_ok());
        let err = gated.handle(Command::StopRecording).unwrap_err();
        assert_eq!(err.code, ErrorCode::PermissionDenied);
        assert_eq!(gated.inner.0, 1);
    }

    #[test]
    fn policies_allow_up_to_their_level() {
        let status = Command::GetStatus;
        let create = Command::CreateScene {
            name: "Show".to_owned(),
        };
        let stop = Command::StopRecording;
        let preview = Command::SetPreview { enabled: true };
        let read_only = AccessPolicy::READ_ONLY;
        assert!(read_only.authorize(&status).is_ok());
        let denied = read_only.authorize(&create).unwrap_err();
        assert_eq!(denied.code, ErrorCode::PermissionDenied);
        assert_eq!(
            denied.details.as_ref().expect("details")["required"],
            "control"
        );
        let control = AccessPolicy::allowing(CommandAccess::Control);
        assert!(control.authorize(&create).is_ok() && control.authorize(&stop).is_ok());
        assert!(control.authorize(&preview).is_err());
        assert!(
            AccessPolicy::allowing(CommandAccess::Sensitive)
                .authorize(&preview)
                .is_ok()
        );
    }
}
