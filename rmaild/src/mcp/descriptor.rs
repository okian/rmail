//! An index over the compiled `FileDescriptorSet` (task 53).
//!
//! Everything the MCP projection knows about the *shape* of an RPC comes from
//! here, and nothing about that shape is written down a second time. The
//! parity registry (`rmail_core::parity`) deliberately does not carry whether
//! a method streams or what its request message looks like — see its own
//! module docs — precisely because both are already in the bytes
//! `tonic-build` emitted, and a second copy could only drift.
//!
//! # Why an index rather than a walk per lookup
//!
//! `FILE_DESCRIPTOR_SET` is ~100 methods across ~20 files, and a projection
//! resolves a request message, then every message and enum it references,
//! transitively. Re-scanning the file list for each of those is quadratic in
//! a way that is invisible at this size and unpleasant at ten times it, so
//! the set is decoded once into name-keyed maps behind a [`OnceLock`] and
//! shared for the process's lifetime.
//!
//! # Names carry no leading dot here
//!
//! `FieldDescriptorProto::type_name` is emitted fully qualified *with* a
//! leading `.` (`.rmail.v1.Message`), while `FileDescriptorProto::package`
//! and the names a human writes do not have one. Storing both forms would be
//! two conventions in one map; [`Catalog`] normalizes to the dotless form on
//! insert and [`Catalog::message`]/[`Catalog::enumeration`] strip a leading
//! dot on lookup, so a caller may pass either.

use std::collections::HashMap;
use std::sync::OnceLock;

use prost::Message as _;
use prost_types::{DescriptorProto, EnumDescriptorProto, FileDescriptorSet};

use super::McpError;

/// One method of one service, as the compiled protos describe it.
#[derive(Debug, Clone)]
pub struct Method {
    /// The gRPC path (`/rmail.v1.MailService/Get`) — the same string
    /// [`rmail_core::parity::Command::rpc`] returns and
    /// `crate::auth::methods::lookup` is keyed by. That the three agree is
    /// what makes the projection a join rather than three parallel tables.
    pub path: String,
    /// Fully-qualified request message name, without a leading dot.
    pub input_type: String,
    /// Fully-qualified response message name, without a leading dot.
    pub output_type: String,
    /// Whether the server may send more than one response message.
    pub server_streaming: bool,
    /// Whether the client may send more than one request message.
    ///
    /// No RPC in this workspace does today, and the projection refuses to
    /// build a tool for one that did (see `projection::ToolSurface::build`):
    /// a single `tools/call` has one argument object, so there is nothing
    /// honest to send as the second request message.
    pub client_streaming: bool,
}

/// Name-keyed views of the compiled descriptor set.
#[derive(Debug)]
pub struct Catalog {
    messages: HashMap<String, DescriptorProto>,
    enums: HashMap<String, EnumDescriptorProto>,
    methods: Vec<Method>,
}

impl Catalog {
    /// Every method the compiled protos declare, in file/service/method
    /// declaration order.
    #[must_use]
    pub fn methods(&self) -> &[Method] {
        &self.methods
    }

    /// The descriptor for a message type, with or without a leading dot.
    #[must_use]
    pub fn message(&self, name: &str) -> Option<&DescriptorProto> {
        self.messages.get(name.trim_start_matches('.'))
    }

    /// The descriptor for an enum type, with or without a leading dot.
    #[must_use]
    pub fn enumeration(&self, name: &str) -> Option<&EnumDescriptorProto> {
        self.enums.get(name.trim_start_matches('.'))
    }

    /// Decode `bytes` (an encoded `FileDescriptorSet`) into an index.
    ///
    /// Taking the bytes rather than reading `rmail_proto::FILE_DESCRIPTOR_SET`
    /// directly is what lets `codec`'s tests exercise shapes no `rmail.v1`
    /// message has yet — a `map<K, V>` field, most importantly — against a
    /// synthetic descriptor set rather than leaving that code path untested
    /// until a proto grows one.
    ///
    /// # Errors
    ///
    /// [`McpError::Descriptor`] if the bytes are not a decodable
    /// `FileDescriptorSet`. For the compiled-in set this cannot happen at
    /// runtime — it would be a build failure — but the projection has no
    /// business panicking over it either way.
    ///
    /// `pub` rather than `pub(super)` because `mail api call` builds one of
    /// these from bytes the *daemon's reflection service* just handed it, not
    /// from `rmail_proto::FILE_DESCRIPTOR_SET`. That is the whole point of
    /// reaching an RPC through reflection: the shape used to encode the
    /// request is the shape the process on the other end of the socket says
    /// it serves, so a `mail` built against a different revision of the protos
    /// than the `rmaild` it is talking to fails on the method it cannot find
    /// rather than silently encoding a field number that has moved.
    pub fn build(bytes: &[u8]) -> Result<Self, McpError> {
        let set = FileDescriptorSet::decode(bytes)
            .map_err(|e| McpError::Descriptor(format!("the compiled descriptor set: {e}")))?;

        let mut messages = HashMap::new();
        let mut enums = HashMap::new();
        let mut methods = Vec::new();

        for file in &set.file {
            let package = file.package();
            for message in &file.message_type {
                index_message(package, message, &mut messages, &mut enums);
            }
            for enumeration in &file.enum_type {
                enums.insert(qualify(package, enumeration.name()), enumeration.clone());
            }
            for service in &file.service {
                let service_name = qualify(package, service.name());
                for method in &service.method {
                    methods.push(Method {
                        path: format!("/{service_name}/{}", method.name()),
                        input_type: method.input_type().trim_start_matches('.').to_owned(),
                        output_type: method.output_type().trim_start_matches('.').to_owned(),
                        server_streaming: method.server_streaming(),
                        client_streaming: method.client_streaming(),
                    });
                }
            }
        }

        if methods.is_empty() {
            return Err(McpError::Descriptor(
                "the compiled descriptor set declares no services".to_owned(),
            ));
        }
        Ok(Self {
            messages,
            enums,
            methods,
        })
    }
}

/// The index over this workspace's own compiled protos.
///
/// # Errors
///
/// [`McpError::Descriptor`] if `rmail_proto::FILE_DESCRIPTOR_SET` does not
/// decode. The failure is not cached: it is deterministic, so re-deriving it
/// costs one decode of a few tens of kilobytes on a path that is already
/// failing, and caching it would mean holding a `Result` in the `OnceLock`
/// and handing callers a reference to an error they cannot own.
pub fn catalog() -> Result<&'static Catalog, McpError> {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    if let Some(catalog) = CATALOG.get() {
        return Ok(catalog);
    }
    let built = Catalog::build(rmail_proto::FILE_DESCRIPTOR_SET)?;
    Ok(CATALOG.get_or_init(|| built))
}

/// Register `message` and, recursively, everything nested inside it.
///
/// Nested types are registered under their own qualified names
/// (`rmail.v1.Outer.Inner`) *as well as* remaining reachable through their
/// parent's `nested_type` list, because that is how `type_name` refers to
/// them — a field of type `.rmail.v1.Outer.Inner` must resolve by name alone,
/// without the resolver knowing which message it was declared inside.
fn index_message(
    prefix: &str,
    message: &DescriptorProto,
    messages: &mut HashMap<String, DescriptorProto>,
    enums: &mut HashMap<String, EnumDescriptorProto>,
) {
    let name = qualify(prefix, message.name());
    for nested in &message.nested_type {
        index_message(&name, nested, messages, enums);
    }
    for enumeration in &message.enum_type {
        enums.insert(qualify(&name, enumeration.name()), enumeration.clone());
    }
    messages.insert(name, message.clone());
}

/// `("rmail.v1", "Message")` -> `"rmail.v1.Message"`; an empty prefix (the
/// unpackaged case) yields the bare name rather than a leading dot.
fn qualify(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}.{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_compiled_descriptor_set_indexes() {
        let catalog = catalog().expect("the compiled descriptor set must decode");
        assert!(
            catalog.methods().len() > 90,
            "only {} methods indexed",
            catalog.methods().len()
        );
    }

    #[test]
    fn a_method_path_matches_the_parity_registry_spelling() {
        let catalog = catalog().expect("catalog");
        let method = catalog
            .methods()
            .iter()
            .find(|m| m.path == "/rmail.v1.MailService/Get")
            .expect("MailService/Get is served");
        assert_eq!(method.input_type, "rmail.v1.GetMessageRequest");
        assert!(!method.server_streaming);
        assert!(!method.client_streaming);
    }

    #[test]
    fn server_streaming_is_read_off_the_descriptor() {
        let catalog = catalog().expect("catalog");
        let list = catalog
            .methods()
            .iter()
            .find(|m| m.path == "/rmail.v1.MailService/List")
            .expect("MailService/List is served");
        assert!(list.server_streaming);
    }

    #[test]
    fn nested_and_imported_types_resolve_by_qualified_name() {
        let catalog = catalog().expect("catalog");
        // An imported well-known type: several RPCs return it.
        assert!(catalog.message("google.protobuf.Empty").is_some());
        // Both spellings of the same name resolve.
        assert!(catalog.message(".rmail.v1.GetMessageRequest").is_some());
        assert!(catalog.message("rmail.v1.GetMessageRequest").is_some());
        // An enum declared at file scope.
        assert!(catalog.enumeration(".rmail.v1.SyncMode").is_some());
    }

    #[test]
    fn a_malformed_descriptor_set_is_an_error_not_a_panic() {
        let error = Catalog::build(&[0xff, 0xff, 0xff]).expect_err("garbage must not decode");
        assert!(matches!(error, McpError::Descriptor(_)), "{error:?}");
    }
}
