//! Windows Service integration using the `windows-service` crate.
//! This module is only compiled on Windows.

#[cfg(target_os = "windows")]
mod imp {
    use std::ffi::OsString;
    use std::time::Duration;

    use tracing::{error, info};
    use windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    use windows_service::service_dispatcher;

    use crate::shutdown::ShutdownController;

    pub const SERVICE_NAME: &str = "MyOwnBackup";
    pub const SERVICE_DISPLAY_NAME: &str = "My Own Backup Service";
    pub const SERVICE_DESCRIPTION: &str =
        "Block-level backup service for My Own Backup";

    windows_service::define_windows_service!(ffi_service_main, service_main);

    pub fn run_as_service() -> Result<(), Box<dyn std::error::Error>> {
        service_dispatcher::start(SERVICE_NAME, ffi_service_main)?;
        Ok(())
    }

    fn service_main(arguments: Vec<OsString>) {
        if let Err(e) = run_service(arguments) {
            error!("Service failed: {}", e);
        }
    }

    fn run_service(_arguments: Vec<OsString>) -> Result<(), Box<dyn std::error::Error>> {
        let (shutdown_controller, shutdown_signal) = crate::shutdown::create_shutdown_channel();

        let event_handler = {
            let controller = shutdown_controller;
            move |control_event| -> ServiceControlHandlerResult {
                match control_event {
                    ServiceControl::Stop => {
                        info!("Service stop requested");
                        controller.trigger_shutdown();
                        ServiceControlHandlerResult::NoError
                    }
                    ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                    _ => ServiceControlHandlerResult::NotImplemented,
                }
            }
        };

        let status_handle =
            service_control_handler::register(SERVICE_NAME, event_handler)?;

        // Report Running
        status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Running,
            controls_accepted: ServiceControlAccept::STOP,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        })?;

        info!("Service started");

        // Run the main application loop
        let runtime = tokio::runtime::Runtime::new()?;
        runtime.block_on(crate::run_main_loop(shutdown_signal))?;

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
