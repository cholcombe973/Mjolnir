use protoc_rust;

fn main() {
    protoc_rust::run(protoc_rust::Args {
        out_dir: "src/proto",
        input: &[
            "protos/agent.proto",
            "protos/plugin.proto",
            "protos/mjolnir.proto",
        ],
        includes: &[],
    }).expect("protoc");
}
