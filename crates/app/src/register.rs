//! File-association registration (`aiframes --register` / `--unregister`).

use suite_shell::AppRegistration;

const APP: AppRegistration = AppRegistration {
    name: "AIFrames",
    exe_name: "aiframes.exe",
    prog_id: "AIFrames.Project",
    type_name: "AIFrames Project",
    description: "AI-first video editor with a built-in MCP server: edit while Claude edits live",
    extensions: &[".aifr"],
};

pub fn register() -> Result<(), String> {
    let exe = suite_shell::register(&APP)?;
    println!("Registered AIFrames ({exe}) for: {}", APP.extensions.join(" "));
    Ok(())
}

pub fn unregister() -> Result<(), String> {
    suite_shell::unregister(&APP)?;
    println!("Removed AIFrames file associations.");
    Ok(())
}
