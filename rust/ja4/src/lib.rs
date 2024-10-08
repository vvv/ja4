// Copyright (c) 2023, FoxIO, LLC.
// All rights reserved.
// Patent Pending
// JA4 is Open-Source, Licensed under BSD 3-Clause
// JA4+ (JA4S, JA4H, JA4L, JA4X, JA4SSH) are licenced under the FoxIO License 1.1.
// For full license text, see the repo root.

mod conf;
mod error;
mod http;
mod pcap;
mod ssh;
mod stream;
mod time;
mod tls;

use std::{io::Write, path::PathBuf};

use clap::Parser;
use rtshark::RTSharkBuilder;

pub use crate::error::Error;
use crate::{
    conf::Conf,
    pcap::{Packet, PacketNum, Proto},
    stream::Streams,
};

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Calculate JA4 fingerprints
#[derive(Debug, Parser)]
#[command(version = env!("CARGO_PKG_VERSION"))]
pub struct Cli {
    /// JSON output (default is YAML)
    #[arg(short, long)]
    json: bool,
    /// Include raw (unhashed) fingerprints in the output
    #[arg(short = 'r', long)]
    with_raw: bool,
    /// Preserve the original order of values.
    ///
    /// JA4 (TLS client): disable sorting of ciphers and TLS extensions.
    ///
    /// JA4H (HTTP client): disable sorting of headers and cookies.
    #[arg(short = 'O', long)]
    original_order: bool,
    /// The key log file that enables decryption of TLS traffic.
    ///
    /// This file is generated by the browser when `SSLKEYLOGFILE` environment variable is set.
    /// See <https://wiki.wireshark.org/TLS#using-the-pre-master-secret> for more details.
    ///
    /// Note that you can embed the TLS key log file in a capture file:
    /// `editcap --inject-secrets tls,keys.txt in.pcap out-dsb.pcapng`
    #[arg(long)]
    keylog_file: Option<PathBuf>,
    /// Include packet numbers (`pkt_*` fields) in the output.
    ///
    /// This information is useful for debugging.
    #[arg(short = 'n', long)]
    with_packet_numbers: bool,
    /// The capture file to process
    pcap: PathBuf,
}

impl Cli {
    /// Write JSON with JA4 fingerprints to the I/O stream.
    pub fn run<W: Write>(self, writer: &mut W) -> Result<()> {
        let conf = Conf::load()?;
        let Cli {
            json,
            with_raw,
            original_order,
            keylog_file,
            with_packet_numbers,
            pcap,
        } = self;

        let Some(pcap_path) = pcap.to_str() else {
            return Err(Error::NonUtf8Path(pcap));
        };
        check_tshark_version()?;
        let mut builder = RTSharkBuilder::builder().input_path(pcap_path);

        if let Some(keylog) = &keylog_file {
            let Some(keylog_path) = keylog.to_str() else {
                // SAFETY: we've just established that `keylog_file` is Some
                return Err(Error::NonUtf8Path(keylog_file.unwrap()));
            };
            builder = builder.keylog_file(keylog_path);
        }
        let mut tshark = builder.spawn()?;

        let mut streams = Streams::default();

        let mut packet_num = 0;
        while let Some(packet) = tshark.read().unwrap_or_else(|err| {
            tracing::error!(%err, "failed to parse tshark output");
            None
        }) {
            packet_num += 1;
            let pkt = Packet::new(&packet, packet_num);

            if let Err(error) = streams.update(&pkt, &conf, with_packet_numbers) {
                tracing::debug!(packet_num, %error, "failed to handle packet");
            }
        }

        let flags = FormatFlags {
            with_raw,
            original_order,
        };
        // HACK: The purpose of the `io::stdout` mumbo-jumbo is to handle
        // BrokenPipe error. Rust throws it when the stdout is piped to `head`.
        if json {
            for rec in streams.into_out(flags) {
                serde_json::to_writer(&mut *writer, &rec)?;
                writeln!(writer)?;
            }
        } else {
            let s = serde_yaml::to_string(&streams.into_out(flags).collect::<Vec<_>>())?;
            writer.write_all(s.as_bytes())?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct FormatFlags {
    /// Whether to add raw (unhashed) fingerprints to the output.
    ///
    /// Corresponds to [`Cli::with_raw`].
    pub(crate) with_raw: bool,
    /// Whether to preserve the original order of values.
    ///
    /// Corresponds to [`Cli::original_order`].
    pub(crate) original_order: bool,
}

/// Which side of the connection sent the packet?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Sender {
    Client,
    Server,
}

/// Returns first 12 characters of the SHA-256 hash of the given string.
///
/// Returns `"000000000000"` (12 zeros) if the input string is empty.
fn hash12(s: impl AsRef<str>) -> String {
    use sha2::{Digest as _, Sha256};

    let s = s.as_ref();
    if s.is_empty() {
        "000000000000".to_owned()
    } else {
        let sha256 = hex::encode(Sha256::digest(s));
        sha256[..12].into()
    }
}

#[test]
fn test_hash12() {
    assert_eq!(hash12("551d0f,551d25,551d11"), "aae71e8db6d7");
    assert_eq!(hash12(""), "000000000000");
}

fn check_tshark_version() -> Result<()> {
    use owo_colors::OwoColorize as _;

    let out = duct::cmd!("tshark", "--version")
        .read()
        .map_err(|e| Error::TsharkNotFound { source: e })?;
    tracing::debug!(%out, "tshark --version");

    let ver = parse_tshark_version(&out).ok_or(Error::ParseTsharkVersion)?;
    let available = semver::Version::parse(ver)?;

    let required = semver::VersionReq::parse(">=4.0.6").expect("BUG");
    if !required.matches(&available) {
        tracing::warn!(%available, %required, "tshark version is outdated");
        let warning = format!(
            "⚠️  You are running an older version of tshark ({available}).\n\
            JA4 is designed to work with tshark version 4.0.6 and above.\n\
            Some functionality may not work properly with older versions."
        );
        eprintln!("{}", warning.bold().red());
    }
    Ok(())
}

/// Parses the version number from the output of `tshark --version`.
fn parse_tshark_version(tshark_version_output: &str) -> Option<&str> {
    // The first line of `tshark --version` output is formatted like this:
    // "TShark (Wireshark) 4.0.8 (v4.0.8-0-g81696bb74857).\n"
    let start = tshark_version_output.find(") ").map(|i| i + 2)?;
    let version_start = &tshark_version_output[start..];
    let end = version_start.find(char::is_whitespace)?;
    let ver = &version_start[..end];
    Some(ver.strip_suffix('.').unwrap_or(ver))
}

#[test]
fn test_parse_tshark_version() {
    assert_eq!(
        parse_tshark_version("TShark (Wireshark) 4.0.8 (v4.0.8-0-g81696bb74857)."),
        Some("4.0.8")
    );
    assert_eq!(
        parse_tshark_version("TShark (Wireshark) 3.6.2 (Git v3.6.2 packaged as 3.6.2-2)"),
        Some("3.6.2")
    );
    assert_eq!(
        parse_tshark_version("TShark (Wireshark) 4.4.0.\n\nCopyright 1998-2024"),
        Some("4.4.0")
    );
    // Abrupt end of the string.
    assert!(parse_tshark_version("TShark (Wireshark) 4.4.0.").is_none());
    assert!(parse_tshark_version("What the TShark?!").is_none());
}

// XXX-FIXME(vvv): `test_insta` fails on Windows; see https://github.com/FoxIO-LLC/ja4/issues/10
#[cfg(not(windows))]
#[test]
fn test_insta() {
    insta::glob!(
        concat!(env!("CARGO_MANIFEST_DIR"), "/../.."),
        "pcap/*.pcap*",
        |path| {
            let cli = Cli {
                json: false,
                with_raw: false,
                original_order: false,
                keylog_file: None,
                with_packet_numbers: false,
                pcap: path.to_path_buf(),
            };

            let mut output = Vec::<u8>::new();
            cli.run(&mut output).unwrap();
            let output = String::from_utf8(output).unwrap();

            insta::assert_snapshot!(output);
        }
    );
}
