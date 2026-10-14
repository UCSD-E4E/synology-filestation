//! What the tunnel does when it cannot do what was asked.
//!
//! The happy path needs a server: an OpenVPN daemon to hand a real handshake
//! to, and a NAS behind it. Both exist — the openvpn crate drives an actual
//! `openvpn` process, and completes a whole TCP conversation against an
//! in-process peer — and neither belongs here. What belongs here is the layer
//! this crate adds: reading the profile, turning a decision's address into a
//! destination, and failing in a way that says which of those went wrong.
//!
//! That matters more than it sounds. Every one of these reaches a user as "the
//! tunnel didn't come up", and the difference between a profile that was never
//! fetched and a NAS that is not answering is the difference between a fix and
//! a shrug.

use std::time::Duration;

use synology_filestation_connect::{OpenVpnTunnel, Tunnel, VpnMtu};

/// A profile shaped like the one `synology_vpn` publishes, pointed wherever
/// the test needs.
///
/// The certificate is generated rather than invented: a `<ca>` block that does
/// not parse fails the attempt in under a millisecond, which would make the
/// bounded-wait test below pass without waiting for anything at all.
fn profile_for(remote: &str, port: u16) -> String {
    let ca = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("a certificate")
        .cert
        .pem();
    let key = (0..16)
        .map(|_| "0123456789abcdef0123456789abcdef")
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "client\n\
         dev tun\n\
         proto udp\n\
         remote {remote} {port}\n\
         auth-user-pass\n\
         cipher AES-256-CBC\n\
         <ca>\n\
         {ca}\
         </ca>\n\
         key-direction 1\n\
         <tls-auth>\n\
         -----BEGIN OpenVPN Static key V1-----\n\
         {key}\n\
         -----END OpenVPN Static key V1-----\n\
         </tls-auth>\n"
    )
}

fn written(text: &str) -> tempfile::NamedTempFile {
    use std::io::Write;
    let mut file = tempfile::NamedTempFile::new().expect("a temp file");
    file.write_all(text.as_bytes()).expect("written");
    file
}

fn tunnel_for(profile: &std::path::Path, patience: Duration) -> OpenVpnTunnel {
    OpenVpnTunnel::new(profile, "ad-user", "hunter2", None, patience)
}

#[tokio::test]
async fn a_profile_that_is_not_there_says_so() {
    // The commonest one: the fetch has not happened, or happened somewhere
    // else. Reported as "the tunnel didn't come up", a user goes looking at
    // the network.
    let tunnel = tunnel_for(
        std::path::Path::new("/nowhere/e4e-nas-vpn.ovpn"),
        Duration::from_secs(5),
    );

    let Err(refused) = tunnel.open("10.90.24.1", 445).await else {
        panic!("there is no profile to open one with");
    };

    let said = refused.to_string();
    assert!(
        said.contains("profile") && said.contains("e4e-nas-vpn.ovpn"),
        "it names the file it could not read: {said}"
    );
}

#[tokio::test]
async fn a_profile_that_will_not_parse_says_that_instead() {
    let file = written("this is not an ovpn file at all\n");
    let tunnel = tunnel_for(file.path(), Duration::from_secs(5));

    let Err(refused) = tunnel.open("10.90.24.1", 445).await else {
        panic!("that is not a profile");
    };

    assert!(
        refused.to_string().contains("profile"),
        "the profile is what is at fault, not the network: {refused}"
    );
}

#[tokio::test]
async fn an_address_that_is_a_name_is_refused_rather_than_resolved() {
    // The tunnel pushes no DNS on purpose, so a name inside it would resolve
    // against the network the tunnel exists to get around — which either fails
    // or, worse, succeeds and points somewhere else entirely.
    let file = written(&profile_for("127.0.0.1", 1194));
    let tunnel = tunnel_for(file.path(), Duration::from_secs(5));

    let Err(refused) = tunnel.open("nas.example.com", 445).await else {
        panic!("that is not an address");
    };

    assert!(
        refused.to_string().contains("address"),
        "it says the destination has to be one: {refused}"
    );
}

#[tokio::test]
async fn a_server_that_never_answers_gives_up_when_it_said_it_would() {
    // The trait asks implementations to bound their own wait, because the HTTP
    // leg is one branch below and somebody is watching a spinner. A tunnel
    // that waits out its own thirty-second handshake timeout has already lost
    // that argument.
    let dead = std::net::UdpSocket::bind("127.0.0.1:0").expect("a socket");
    let port = dead.local_addr().expect("bound").port();
    drop(dead);

    let file = written(&profile_for("127.0.0.1", port));
    let tunnel = tunnel_for(file.path(), Duration::from_secs(2));

    let started = std::time::Instant::now();
    let Err(refused) = tunnel.open("10.90.24.1", 445).await else {
        panic!("nobody is there to answer");
    };

    // It has to have got as far as trying, or the bound is not what stopped
    // it: a profile that fails to parse would satisfy the ceiling below
    // without waiting for anything.
    assert!(
        started.elapsed() > Duration::from_millis(500),
        "the attempt was made, not skipped: {:?} after {refused}",
        started.elapsed()
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "it gave up in about the time it was given, not the handshake's own: \
         {:?} after {refused}",
        started.elapsed()
    );
}

// An MTU out of range cannot reach `with_mtu` at all: `VpnMtu::new` is the only
// way to make one, and it is tested where it lives, in the openvpn crate.

#[test]
fn a_smaller_mtu_is_kept() {
    let mtu = VpnMtu::new(1150).expect("in range");
    let tunnel = tunnel_for(
        std::path::Path::new("/nowhere.ovpn"),
        Duration::from_secs(5),
    )
    .with_mtu(mtu);

    assert!(
        format!("{tunnel:?}").contains("mtu: Mtu(1150)"),
        "{tunnel:?}"
    );
}

#[test]
fn without_one_the_default_is_used() {
    let tunnel = tunnel_for(
        std::path::Path::new("/nowhere.ovpn"),
        Duration::from_secs(5),
    );

    assert!(
        format!("{tunnel:?}").contains(&format!("mtu: {:?}", VpnMtu::default())),
        "{tunnel:?}"
    );
}
