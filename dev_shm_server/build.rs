fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::compile_protos("proto/shm_transfer.proto")?;

    // Compile encrypted-zone-node protos (EzIsolateBridge, IsolateEzBridge)
    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(
            &[
                "proto/enforcer/v1/ez_isolate_bridge.proto",
                "proto/enforcer/v1/isolate_ez_bridge.proto",
            ],
            &["proto"],
        )?;

    Ok(())
}
