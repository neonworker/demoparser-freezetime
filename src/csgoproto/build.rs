use std::{io::Result, process::Command};

fn main() -> Result<()> {
    println!("cargo::rerun-if-changed=GameTracking-CS2/Protobufs/demo.proto");
    println!("cargo::rerun-if-env-changed=DEMOPARSER_REGEN_PROTOS");

    // Proto regeneration is OPT-IN (freezetime fork patch). By default we
    // compile the committed `src/protobuf.rs` (the prost output) alongside the
    // static `src/maps.rs`. This makes builds reproducible (no silent tracking
    // of SteamDatabase/GameTracking-CS2 HEAD), removes the GameTracking-CS2
    // clone + network dependency from every build, and stops the regenerated
    // file from re-dirtying the tracked tree after each `cargo build`.
    //
    // To refresh the bindings against the latest upstream CS2 protobufs:
    //   DEMOPARSER_REGEN_PROTOS=1 cargo build -p csgoproto
    // then review the `git diff` on `src/protobuf.rs` and commit it.
    if std::env::var_os("DEMOPARSER_REGEN_PROTOS").is_none() {
        return Ok(());
    }

    Command::new("git")
        .args([
            "clone",
            "https://github.com/SteamDatabase/GameTracking-CS2.git",
            "--depth=1",
        ])
        .status()?;

    let protos = vec![
        "GameTracking-CS2/Protobufs/steammessages.proto",
        "GameTracking-CS2/Protobufs/gcsdk_gcmessages.proto",
        "GameTracking-CS2/Protobufs/demo.proto",
        "GameTracking-CS2/Protobufs/cstrike15_gcmessages.proto",
        "GameTracking-CS2/Protobufs/cstrike15_usermessages.proto",
        "GameTracking-CS2/Protobufs/usermessages.proto",
        "GameTracking-CS2/Protobufs/networkbasetypes.proto",
        "GameTracking-CS2/Protobufs/engine_gcmessages.proto",
        "GameTracking-CS2/Protobufs/netmessages.proto",
        "GameTracking-CS2/Protobufs/network_connection.proto",
        "GameTracking-CS2/Protobufs/cs_usercmd.proto",
        "GameTracking-CS2/Protobufs/usercmd.proto",
        "GameTracking-CS2/Protobufs/gameevents.proto",
        "GameTracking-CS2/Protobufs/cs_gameevents.proto",
    ];

    prost_build::Config::new()
        .format(false)
        .out_dir("src")
        .default_package_filename("protobuf")
        .bytes(["."])
        .enum_attribute(".", "#[derive(::strum::EnumIter)]")
        .compile_protos(&protos, &["GameTracking-CS2/Protobufs/"])
}
