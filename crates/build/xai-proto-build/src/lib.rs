mod debug_redact;
pub mod find_protoc;

use anyhow::Context;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::{fs, iter};

/// Find the protoc well-known types include directory.
///
/// When PROTOC is set (e.g., in Bazel), the include directory is typically
/// at `../include` relative to the `bin/protoc` binary. For example:
/// - PROTOC = `/path/to/external/protoc_linux_x86_64/bin/protoc`
/// - Include = `/path/to/external/protoc_linux_x86_64/include`
///
/// This is needed because Bazel places the protoc binary and include files
/// in separate locations within the sandbox, and protoc doesn't automatically
/// find them without an explicit -I flag.
fn find_protoc_include_dir(protoc: Option<&Path>) -> Option<PathBuf> {
    let protoc = protoc?;

    // protoc is typically at .../bin/protoc, so include is at .../include
    let parent = protoc.parent()?; // .../bin
    let grandparent = parent.parent()?; // .../
    let include_dir = grandparent.join("include");

    if include_dir.is_dir() {
        Some(include_dir)
    } else {
        None
    }
}

/// Parse the Makefile-style dependency output emitted by `protoc`.
///
/// `protoc` writes the descriptor output path as the target on the first line.
/// Do not split on `:` here: a Windows target path contains a drive-letter
/// colon. Instead, remove the exact descriptor path that we passed to protoc.
fn parse_protoc_dependencies<'a>(
    dependency_output: &'a str,
    descriptor_output_path: &Path,
) -> anyhow::Result<Vec<&'a str>> {
    let mut lines = dependency_output.lines();
    let first_line = lines.next().context("protoc dependency output is empty")?;
    let expected_prefix = format!("{}:", descriptor_output_path.display());
    let first_dependency = first_line.strip_prefix(&expected_prefix).with_context(|| {
        format!(
            "protoc dependency output must start with {expected_prefix:?}: {dependency_output:?}"
        )
    })?;

    Ok(iter::once(first_dependency)
        .chain(lines)
        .map(str::trim)
        .map(|line| line.strip_suffix('\\').unwrap_or(line).trim())
        .filter(|line| !line.is_empty())
        .collect())
}

fn is_well_known_type_include(path: &str) -> bool {
    // protoc writes native path separators in dependency files. Normalize only
    // for this comparison so the existing well-known-type filtering applies on
    // both Unix and Windows.
    path.replace('\\', "/")
        .contains("/include/google/protobuf/")
}

fn protoc_output_argument(flag: &str, path: &Path) -> OsString {
    let mut argument = OsString::from(flag);
    argument.push(path);
    argument
}

pub struct XaiProtoBuilder {
    builder: tonic_prost_build::Builder,
    file_descriptor_set_path: Option<PathBuf>,
    gen_pbjson: bool,
    pbjson_ignore_unknown_fields: bool,
    pbjson_preserve_proto_field_names: bool,
    honor_debug_redact: bool,
}

impl XaiProtoBuilder {
    fn map_builder(
        self,
        f: impl FnOnce(tonic_prost_build::Builder) -> tonic_prost_build::Builder,
    ) -> Self {
        Self {
            builder: f(self.builder),
            ..self
        }
    }

    pub fn btree_map<S: AsRef<str>>(self, paths: impl IntoIterator<Item = S>) -> Self {
        self.map_builder(|b| paths.into_iter().fold(b, |b, path| b.btree_map(path)))
    }

    pub fn bytes<S: AsRef<str>>(self, paths: impl IntoIterator<Item = S>) -> Self {
        self.map_builder(|b| paths.into_iter().fold(b, |b, path| b.bytes(path)))
    }

    pub fn extern_path(self, proto_path: impl AsRef<str>, rust_path: impl AsRef<str>) -> Self {
        self.map_builder(|b| b.extern_path(proto_path, rust_path))
    }

    pub fn file_descriptor_set_path(mut self, path: impl AsRef<Path>) -> Self {
        self.file_descriptor_set_path = Some(path.as_ref().to_path_buf());
        self.map_builder(|b| b.file_descriptor_set_path(path))
    }

    pub fn gen_pbjson(mut self) -> Self {
        self.gen_pbjson = true;
        self
    }

    pub fn pbjson_ignore_unknown_fields(mut self) -> Self {
        self.pbjson_ignore_unknown_fields = true;
        self
    }

    /// Serialize JSON using the original proto field names (snake_case) instead
    /// of the proto3-JSON default (camelCase). Deserialization still accepts
    /// both casings, so this is backward-compatible with already-stored
    /// camelCase documents.
    pub fn pbjson_preserve_proto_field_names(mut self) -> Self {
        self.pbjson_preserve_proto_field_names = true;
        self
    }

    pub fn generate_default_stubs(self, enable: bool) -> Self {
        self.map_builder(|b| b.generate_default_stubs(enable))
    }

    /// Honor the protobuf `debug_redact` field option: annotated fields
    /// print as `***` in `Debug`. The crate must also depend on `veil`.
    pub fn honor_debug_redact(mut self) -> Self {
        self.honor_debug_redact = true;
        self
    }

    pub fn type_attribute(self, path: impl AsRef<str>, attr: impl AsRef<str>) -> Self {
        self.map_builder(|b| b.type_attribute(path, attr))
    }

    pub fn field_attribute(self, path: impl AsRef<str>, attr: impl AsRef<str>) -> Self {
        self.map_builder(|b| b.field_attribute(path, attr))
    }

    // tonic-build generation of `rerun-if-changed` is lazy and incorrect.
    // - everything is invalidated when anything inside include directories is changed
    // - also they compute paths incorrectly: assuming paths are relative to current directory
    //   rather than
    fn emit_rerun_if_changed<'a>(
        protoc: Option<&Path>,
        protoc_include_dir: Option<&Path>,
        protos: impl IntoIterator<Item = &'a Path>,
        includes: impl IntoIterator<Item = &'a Path>,
    ) -> anyhow::Result<()> {
        let includes = Vec::from_iter(includes);

        if let Some(protoc) = protoc {
            println!(
                "cargo:rerun-if-changed={}",
                protoc.to_str().context("protoc path not UTF-8")?
            );
        }

        // Can only process one input file when using --dependency_out=FILE.
        for proto in protos {
            let temporary_outputs = tempfile::TempDir::new()
                .context("failed to create temporary protoc output directory")?;
            let dependency_output_path = temporary_outputs.path().join("protoc-dependencies.d");
            let descriptor_output_path = temporary_outputs.path().join("protoc-descriptor.pb");
            let mut command = Command::new(protoc.unwrap_or(Path::new("protoc")));
            command
                .arg(protoc_output_argument(
                    "--dependency_out=",
                    &dependency_output_path,
                ))
                .arg(protoc_output_argument(
                    "--descriptor_set_out=",
                    &descriptor_output_path,
                ));

            // Add protoc's well-known types include directory first (if found).
            // This is needed for Bazel sandboxed builds where protoc and its
            // include files are in different locations.
            if let Some(include_dir) = protoc_include_dir {
                command.arg(format!(
                    "-I{}",
                    include_dir.to_str().context("include path not UTF-8")?
                ));
            }

            for include in &includes {
                command.arg(format!("-I{}", include.to_str().context("path not UTF-8")?));
            }

            command.arg(proto);

            command.stdin(Stdio::null());
            command.stderr(Stdio::inherit());
            command.stdout(Stdio::null());

            let status = command.status().context("protoc command failed")?;
            if !status.success() {
                return Err(anyhow::anyhow!("protoc command failed"));
            }

            let dependency_output =
                fs::read_to_string(&dependency_output_path).with_context(|| {
                    format!(
                        "failed to read protoc dependency output {}",
                        dependency_output_path.display()
                    )
                })?;
            for line in parse_protoc_dependencies(&dependency_output, &descriptor_output_path)? {
                // Depending on absolute paths like
                // /Users/user/homebrew/Cellar/protobuf/29.1/include/google/protobuf/timestamp.proto
                // is valid, but we want to have output more deterministic.
                if is_well_known_type_include(line) {
                    continue;
                }

                if !fs::exists(line)? {
                    return Err(anyhow::anyhow!("dependency file not found: {line}"));
                }

                println!("cargo:rerun-if-changed={line}");
            }
        }

        Ok(())
    }

    pub fn compile_protos(
        self,
        protos: &[impl AsRef<Path>],
        includes: &[impl AsRef<Path>],
    ) -> anyhow::Result<()> {
        for proto in protos {
            let proto = proto.as_ref();
            if proto.is_absolute() {
                return Err(anyhow::anyhow!(
                    "Absolute paths are not allowed: {}",
                    proto.display()
                ));
            }
        }

        let XaiProtoBuilder {
            builder,
            gen_pbjson,
            file_descriptor_set_path,
            pbjson_ignore_unknown_fields,
            pbjson_preserve_proto_field_names,
            honor_debug_redact,
        } = self;
        let mut config = prost_build::Config::new();
        config.enable_type_names();

        let protoc = find_protoc::find_protoc()?;

        // Use fixed version of `protoc` binary.
        if let Some(protoc) = &protoc {
            config.protoc_executable(protoc);
        }

        // Find the protoc's well-known types include directory.
        // This is needed for Bazel sandboxed builds where protoc and its
        // include files are placed in different sandbox locations.
        let protoc_include_dir = find_protoc_include_dir(protoc.as_deref());

        let mut builder = builder.emit_rerun_if_changed(false);
        Self::emit_rerun_if_changed(
            protoc.as_deref(),
            protoc_include_dir.as_deref(),
            protos.iter().map(|p| p.as_ref()),
            includes.iter().map(|i| i.as_ref()),
        )?;

        let tempfile;

        let file_descriptor_set_path: Option<PathBuf> =
            if let Some(file_descriptor_set_path) = file_descriptor_set_path {
                Some(file_descriptor_set_path)
            } else if gen_pbjson {
                tempfile = tempfile::TempDir::new()?;
                let file_descriptor_set_path = tempfile.path().join("xai-proto-build.pbbin");
                builder = builder.file_descriptor_set_path(&file_descriptor_set_path);
                Some(file_descriptor_set_path)
            } else {
                None
            };

        // Build the full includes list, prepending the protoc include directory
        // if found (for well-known types like google/protobuf/timestamp.proto).
        let all_includes: Vec<&Path> = protoc_include_dir
            .as_deref()
            .into_iter()
            .chain(includes.iter().map(|i| i.as_ref()))
            .collect();

        let protos: Vec<&Path> = protos.iter().map(|p| p.as_ref()).collect();

        {
            let plain_includes: Vec<&Path> = includes.iter().map(|i| i.as_ref()).collect();
            if honor_debug_redact {
                debug_redact::apply(
                    &mut config,
                    protoc.as_deref(),
                    protoc_include_dir.as_deref(),
                    &plain_includes,
                    &protos,
                )?;
            } else if let Some(field) = debug_redact::first_marked_field(
                protoc.as_deref(),
                protoc_include_dir.as_deref(),
                &plain_includes,
                &protos,
            )? {
                anyhow::bail!(
                    "{field} sets `debug_redact = true` but redaction is not active: \
                     call `.honor_debug_redact()` on the builder"
                );
            }
        }

        builder
            .compile_with_config(config, &protos, &all_includes)
            .context("tonic_build failed")?;

        if gen_pbjson {
            let file_descriptor_set_path =
                file_descriptor_set_path.context("fds must be set at this moment")?;
            let descriptor_set = fs::read(&file_descriptor_set_path).with_context(|| {
                format!(
                    "Failed to read file descriptor set {}",
                    file_descriptor_set_path.display()
                )
            })?;
            let mut builder = pbjson_build::Builder::new();
            builder
                .register_descriptors(&descriptor_set)
                .context("Failed to register descriptors in pbjson_build")?;
            if pbjson_ignore_unknown_fields {
                builder.ignore_unknown_fields();
            }
            if pbjson_preserve_proto_field_names {
                builder.preserve_proto_field_names();
            }
            builder
                .build(&["."])
                .context("Failed to build descriptor set")?;
        }

        Ok(())
    }
}

pub fn configure() -> XaiProtoBuilder {
    let builder = tonic_prost_build::configure()
        .compile_well_known_types(true)
        .extern_path(".google.protobuf", "::pbjson_types")
        .extern_path(".google.protobuf.Empty", "()")
        .protoc_arg("--experimental_allow_proto3_optional");
    XaiProtoBuilder {
        builder,
        gen_pbjson: false,
        pbjson_ignore_unknown_fields: false,
        pbjson_preserve_proto_field_names: false,
        file_descriptor_set_path: None,
        honor_debug_redact: false,
    }
}

#[cfg(test)]
mod tests {
    use super::parse_protoc_dependencies;
    use std::path::Path;

    #[test]
    fn parses_unix_dependency_output() {
        let descriptor = Path::new("/tmp/protoc-descriptor.pb");
        let output = "/tmp/protoc-descriptor.pb: proto/service.proto\n";

        assert_eq!(
            parse_protoc_dependencies(output, descriptor).unwrap(),
            vec!["proto/service.proto"]
        );
    }

    #[test]
    fn parses_windows_drive_letter_dependency_output() {
        let descriptor = Path::new(r"D:\build\protoc-descriptor.pb");
        let output = r"D:\build\protoc-descriptor.pb: D:\work\proto\service.proto";

        assert_eq!(
            parse_protoc_dependencies(output, descriptor).unwrap(),
            vec![r"D:\work\proto\service.proto"]
        );
    }

    #[test]
    fn parses_multiline_dependency_output() {
        let descriptor = Path::new("/tmp/protoc-descriptor.pb");
        let output = concat!(
            "/tmp/protoc-descriptor.pb: proto/service.proto \\\n",
            "  proto/common.proto \\\n",
            "  proto/last.proto\n",
        );

        assert_eq!(
            parse_protoc_dependencies(output, descriptor).unwrap(),
            vec![
                "proto/service.proto",
                "proto/common.proto",
                "proto/last.proto"
            ]
        );
    }
}
