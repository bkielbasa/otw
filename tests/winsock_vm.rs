use std::{
    io::{Read, Write},
    net::TcpListener,
    thread,
};

use otw_vm::{kernel::pe::PeImage, vm::Vm};

const WINSOCK_EXE: &[u8] = include_bytes!("../binaries/guests/WINSOCK.EXE");

// Keep in sync with scripts/build-winsock-pe.py.
const IMAGE_BASE: u32 = 0x0040_0000;
const SECTION_RVA: u32 = 0x1000;
const SOCKADDR_OFF: u32 = 0x390;
const SOCKADDR_VA: u32 = IMAGE_BASE + SECTION_RVA + SOCKADDR_OFF;

#[test]
fn parses_hand_built_winsock_guest() {
    let image = PeImage::parse(WINSOCK_EXE.to_vec()).expect("Winsock PE must parse");

    assert_eq!(image.machine, 0x014c);
    assert_eq!(image.image_base, IMAGE_BASE);
    assert_eq!(image.entry_va(), IMAGE_BASE + SECTION_RVA);
    assert_eq!(image.import_count(), 8);

    let winsock: Vec<String> = image.imports[0]
        .names
        .iter()
        .map(|name| name.display())
        .collect();
    assert_eq!(
        winsock,
        [
            "WSAStartup",
            "socket",
            "connect",
            "send",
            "recv",
            "closesocket",
            "WSACleanup"
        ]
    );
    assert_eq!(image.imports[1].dll, "KERNEL32.dll");
}

#[test]
fn guest_performs_tcp_round_trip_through_iat() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener must bind");
    let port = listener.local_addr().expect("listener address").port();

    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("guest must connect");
        let mut request = [0u8; 4];
        stream
            .read_exact(&mut request)
            .expect("guest must send the request");
        assert_eq!(&request, b"ping");
        stream
            .write_all(&request)
            .expect("echo must reach the guest");
    });

    let image = PeImage::parse(WINSOCK_EXE.to_vec()).expect("Winsock PE must parse");
    let mut machine = Vm::new(image).expect("Winsock VM must map");
    assert_eq!(machine.implemented_imports(), 8);

    // Patch sockaddr_in.sin_port (network byte order) before the guest runs.
    let sockaddr = u32::from_le_bytes([2, 0, (port >> 8) as u8, port as u8]);
    assert!(machine.write_u32(SOCKADDR_VA, sockaddr).is_some());

    let result = machine.run().expect("Winsock guest must exit cleanly");
    assert_eq!(result.exit_code, 0);
    assert_eq!(result.api_calls, 8);

    server.join().expect("host echo server must finish");
}
