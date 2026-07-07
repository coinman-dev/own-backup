//! Windows Service integration using the `windows-service` crate.
//! This module is only compiled on Windows.

#[cfg(target_os = "windows")]
mod imp {
    use std::ffi::OsString;
    use std::time::Duration;

    use tracing::{error, info};
    use windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType, SessionChangeReason,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    use windows_service::service_dispatcher;

    use crate::scheduler::OsEvent;
    use crate::SERVICE_NAME;

    windows_service::define_windows_service!(ffi_service_main, service_main);

    /// `ERROR_FAILED_SERVICE_CONTROLLER_CONNECT` — returned by
    /// `StartServiceCtrlDispatcher` when the process was NOT launched by the SCM
    /// (i.e. a human ran the exe in a console).
    const ERROR_FAILED_SERVICE_CONTROLLER_CONNECT: i32 = 1063;

    /// Run under the Service Control Manager. Returns `Ok(true)` when we were
    /// launched by the SCM and have now stopped; `Ok(false)` when we were run
    /// from a console (so the caller can show info instead).
    pub fn run_as_service() -> Result<bool, Box<dyn std::error::Error>> {
        match service_dispatcher::start(SERVICE_NAME, ffi_service_main) {
            Ok(()) => Ok(true),
            Err(windows_service::Error::Winapi(e))
                if e.raw_os_error() == Some(ERROR_FAILED_SERVICE_CONTROLLER_CONNECT) =>
            {
                Ok(false)
            }
            Err(e) => Err(e.into()),
        }
    }

    fn service_main(arguments: Vec<OsString>) {
        if let Err(e) = run_service(arguments) {
            error!("Service failed: {}", e);
        }
    }

    fn run_service(_arguments: Vec<OsString>) -> Result<(), Box<dyn std::error::Error>> {
        let (shutdown_controller, shutdown_signal) = crate::shutdown::create_shutdown_channel();

        // Forward OS events (logon/logoff/shutdown) from the SCM control handler
        // to the scheduler so `event`-kind jobs can fire on them.
        let (os_tx, os_rx) = tokio::sync::mpsc::unbounded_channel::<OsEvent>();

        let event_handler = {
            let controller = shutdown_controller;
            move |control_event| -> ServiceControlHandlerResult {
                match control_event {
                    ServiceControl::Stop => {
                        info!("Service stop requested");
                        controller.trigger_shutdown();
                        ServiceControlHandlerResult::NoError
                    }
                    // Best-effort: the service is stopping, so a triggered run may
                    // not complete — it never touches existing backups.
                    ServiceControl::Shutdown | ServiceControl::Preshutdown => {
                        info!("System shutdown");
                        let _ = os_tx.send(OsEvent::Shutdown);
                        controller.trigger_shutdown();
                        ServiceControlHandlerResult::NoError
                    }
                    ServiceControl::SessionChange(param) => {
                        match param.reason {
                            SessionChangeReason::SessionLogon => {
                                let _ = os_tx.send(OsEvent::Logon);
                            }
                            SessionChangeReason::SessionLogoff => {
                                let _ = os_tx.send(OsEvent::Logoff);
                            }
                            _ => {}
                        }
                        ServiceControlHandlerResult::NoError
                    }
                    ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                    _ => ServiceControlHandlerResult::NotImplemented,
                }
            }
        };

        let status_handle =
            service_control_handler::register(SERVICE_NAME, event_handler)?;

        // Report Running. Accept session-change (logon/logoff) and shutdown so
        // event-triggered jobs can react to them.
        status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Running,
            controls_accepted: ServiceControlAccept::STOP
                | ServiceControlAccept::SHUTDOWN
                | ServiceControlAccept::SESSION_CHANGE,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        })?;

        info!("Service started");

        // Run the main application loop. use_tcp=false → production transport:
        // a DACL-restricted named pipe (no listening TCP port). Foreground/dev
        // (`--foreground`) still uses loopback TCP.
        let runtime = tokio::runtime::Runtime::new()?;
        runtime.block_on(crate::run_main_loop(shutdown_signal, false, os_rx))?;

        // Report Stopped
        status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Stopped,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        })?;

        info!("Service stopped");
        Ok(())
    }
}

#[cfg(target_os = "windows")]
pub use imp::*;
