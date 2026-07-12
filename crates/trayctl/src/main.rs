use std::env;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};

use tray_ipc::{IpcRequest, IpcResponse, send_request, socket_path};

const USAGE: &str = "Usage:
  trayctl [--json] list
  trayctl [--json] activate TARGET
  trayctl [--json] secondary-activate TARGET
  trayctl [--json] context-menu TARGET
  trayctl [--json] menu-next TARGET
  trayctl [--json] menu-previous TARGET
  trayctl [--json] menu-activate TARGET
  trayctl [--json] menu-click TARGET ENTRY_ID
  trayctl [--json] close-menus
  trayctl socket-path

TARGET is a zero-based index from `trayctl list`, an exact item title, or an
exact item key. Set GTK_STATUS_BAR_SOCKET to override the default socket path.";

fn parse_request(arguments: &[String]) -> Result<Option<IpcRequest>> {
    let Some(command) = arguments.first() else {
        bail!(USAGE);
    };
    let request = match command.as_str() {
        "list" if arguments.len() == 1 => IpcRequest::List,
        "activate" => IpcRequest::Activate {
            target: target(arguments, command)?,
        },
        "secondary-activate" => IpcRequest::SecondaryActivate {
            target: target(arguments, command)?,
        },
        "context-menu" => IpcRequest::ContextMenu {
            target: target(arguments, command)?,
        },
        "menu-next" | "menu-down" => IpcRequest::MenuNext {
            target: target(arguments, command)?,
        },
        "menu-previous" | "menu-up" => IpcRequest::MenuPrevious {
            target: target(arguments, command)?,
        },
        "menu-activate" => IpcRequest::MenuActivate {
            target: target(arguments, command)?,
        },
        "menu-click" if arguments.len() == 3 => IpcRequest::MenuClick {
            target: arguments[1].clone(),
            entry: arguments[2]
                .parse()
                .with_context(|| format!("invalid menu entry ID {:?}", arguments[2]))?,
        },
        "close-menus" if arguments.len() == 1 => IpcRequest::CloseMenus,
        "socket-path" if arguments.len() == 1 => return Ok(None),
        "help" | "--help" | "-h" => bail!(USAGE),
        _ => bail!("unknown or malformed command {command:?}\n\n{USAGE}"),
    };
    Ok(Some(request))
}

fn target(arguments: &[String], command: &str) -> Result<String> {
    if arguments.len() != 2 {
        bail!("{command} requires exactly one TARGET\n\n{USAGE}");
    }
    Ok(arguments[1].clone())
}

fn print_human(response: &IpcResponse) {
    for item in &response.items {
        let title = if item.title.is_empty() {
            "<untitled>"
        } else {
            &item.title
        };
        println!(
            "{}\t{}\t{}\t{}\t{}",
            item.index,
            title,
            item.status,
            if item.item_is_menu {
                "menu"
            } else {
                "activate"
            },
            item.key,
        );
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<ExitCode> {
    let mut arguments: Vec<String> = env::args().skip(1).collect();
    let json = match arguments.iter().position(|argument| argument == "--json") {
        Some(index) => {
            arguments.remove(index);
            true
        }
        None => false,
    };
    if matches!(arguments.as_slice(), [argument] if matches!(argument.as_str(), "help" | "--help" | "-h"))
    {
        println!("{USAGE}");
        return Ok(ExitCode::SUCCESS);
    }
    let Some(request) = parse_request(&arguments)? else {
        println!("{}", socket_path()?.display());
        return Ok(ExitCode::SUCCESS);
    };

    let response = send_request(&request).await?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&response).context("encode output as JSON")?
        );
    } else if response.ok {
        print_human(&response);
    } else if let Some(error) = &response.error {
        eprintln!("trayctl: {error}");
    } else {
        eprintln!("trayctl: request failed without an error message");
    }

    Ok(if response.ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(2)
    })
}
