/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::borrow::Cow;

use configparser::config::ConfigSet;
use thiserror::Error;
use thrift_types::edenfs as eden;

#[derive(Debug, Error)]
#[error("cannot decode arguments")]
pub struct NonUTF8Arguments;

pub use cliparser::errors::InvalidArguments;

#[derive(Debug, Error)]
#[error("unknown command '{0}'\n(use 'hg help' to get help)")]
pub struct UnknownCommand(pub String);

/// Explicitly fallback to Python code path.
///
/// Ideally this does not exist.
#[derive(Debug, Error)]
#[error("{0}")]
pub struct FallbackToPython(pub &'static str);

#[derive(Debug, Error)]
#[error("")]
pub struct FailedFallbackToPython;

#[derive(Debug, Error)]
#[error(
    "'{0}' is not inside a repository, but this command requires a repository!\n(use 'cd' to go to a directory inside a repository and try again)"
)]
pub struct RepoRequired(pub String);

#[derive(Debug, Error)]
#[error("malformed --config option: '{0}' (use --config section.name=value)")]
pub struct MalformedConfigOption(pub String);

#[derive(Debug, Error)]
#[error("{0}")]
pub struct Abort(pub Cow<'static, str>);

/// Print an error suitable for end-user consumption.
///
/// This function adds `hg:` or `abort:` to error messages.
pub fn print_error(err: &anyhow::Error, io: &crate::io::IO, _args: &[String]) {
    use cliparser::parser::ParseError;
    if err.downcast_ref::<configparser::Error>().is_some() {
        let _ = io.write_err(format!("hg: parse error: {}\n", err));
    } else if err.downcast_ref::<configparser::Errors>().is_some() {
        let _ = io.write_err(format!("hg: parse errors: {}\n", err));
    } else if let Some(ParseError::AmbiguousCommand {
        command_name: _,
        possibilities,
    }) = err.downcast_ref::<ParseError>()
    {
        let _ = io.write_err(format!("hg: {}:\n", err));
        for possibility in possibilities {
            // UX: Colorize the output once `io` can output colors.
            let _ = io.write_err(format!("     {}\n", possibility));
        }
    } else if let Some(eden::errors::eden_service::GetScmStatusV2Error::ex(e)) =
        err.downcast_ref::<eden::errors::eden_service::GetScmStatusV2Error>()
    {
        let _ = io.write_err(format!("abort: {}\n", e.message));
        let _ = io.flush();
    } else {
        let _ = io.write_err(format!("abort: {}\n", err));
    }
}

/// Optionally transform an error into something more friendly to the user.
pub fn triage_error(config: &ConfigSet, cmd_err: anyhow::Error) -> anyhow::Error {
    if types::errors::is_network_error(&cmd_err)
        && config
            .get_or_default("experimental", "network-doctor")
            .unwrap_or(false)
    {
        match network_doctor::Doctor::new().diagnose(config) {
            Ok(()) => cmd_err,
            Err(diagnosis) =>
            // TODO: colorize diagnosis, vary output by verbose/quiet
            {
                anyhow::anyhow!(
                    "command failed due to network error\n\n{}\n\nDetails:\n\n{:?}\n\nOriginal error:\n\n{:?}\n",
                    diagnosis.treatment(config),
                    diagnosis,
                    cmd_err
                )
            }
        }
    } else {
        if let Some(FallbackToPython(command_name)) = cmd_err.downcast_ref::<FallbackToPython>() {
            if config
                .get_or_default::<Vec<String>>("commands", "force-rust")
                .map_or(false, |config| config.contains(&command_name.to_string()))
            {
                return anyhow::Error::new(FailedFallbackToPython);
            }
        }
        cmd_err
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn test_status_error_msg() {
        // Construct error and parameters
        let error_msg = "cannot compute status while a checkout is currently in progress";
        let expected_error = format!("abort: {}\n", error_msg);

        let error: anyhow::Error =
            eden::errors::eden_service::GetScmStatusV2Error::ex(eden::EdenError {
                message: error_msg.to_string(),
                errorCode: Some(255),
                errorType: eden::EdenErrorType::CHECKOUT_IN_PROGRESS,
                ..Default::default()
            })
            .into();

        let tin = Cursor::new(Vec::new());
        let tout = Cursor::new(Vec::new());
        let terr = Cursor::new(Vec::new());
        let mut io = crate::io::IO::new(tin, tout, Some(terr));

        // Call print_error with error and in-memory IO stream
        print_error(&error, &mut io, &[] as &[String]);

        // Make sure error message is formatted correctly.
        io.with_error(|e| {
            if let Some(actual_error_wrapped) = e {
                let any = actual_error_wrapped.as_any();
                if let Some(c) = any.downcast_ref::<std::io::Cursor<Vec<u8>>>() {
                    let actual_error = c.clone().into_inner();
                    assert_eq!(String::from_utf8(actual_error).unwrap(), expected_error);
                }
            }
        });
    }
}
