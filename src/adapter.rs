use crate::log::AdapterLoggingLevel;
use crate::util;
/// Representation of a wireGuard adapter with safe idiomatic bindings to the functionality provided by
/// the WireGuard* C functions.
///
/// The [`Adapter::create`] and [`Adapter::open`] functions serve as the entry point to using
/// wireguard functionality
use crate::util::UnsafeHandle;
use crate::wireguard_nt_raw;
use crate::WireGuardError;

use std::net::SocketAddr;
use std::ptr;
use std::sync::Arc;

use ipnet::IpNet;
use ipnet::Ipv4Net;
use rand::Rng;
use widestring::U16CString;

/// Wrapper around a `WIREGUARD_ADAPTER_HANDLE`
///
/// Related functions from WireGuardXXX are functions with an adapter self parameter
pub struct Adapter {
    adapter: UnsafeHandle<wireguard_nt_raw::WIREGUARD_ADAPTER_HANDLE>,
    wireguard: Arc<wireguard_nt_raw::wireguard>,
}

/// Representation of a WireGuard peer when setting the config
pub struct SetPeer {
    /// The peer's public key
    pub public_key: Option<[u8; 32]>,

    /// A preshared key used to symmetrically encrypt data with this peer
    pub preshared_key: Option<[u8; 32]>,

    /// How often to send a keep alive packet to prevent NATs from blocking UDP packets
    ///
    /// Set to None if no keep alive behavior is wanted
    pub keep_alive: Option<u16>,

    /// The address this peer is reachable from using UDP across the internet
    pub endpoint: SocketAddr,

    /// The set of [`IpNet`]'s that dictate what packets are allowed to be sent of received from
    /// this peer
    pub allowed_ips: Vec<IpNet>,
}

pub type RebootRequired = bool;

/// The data required when setting the config for an interface
pub struct SetInterface {
    /// The port this interface should listen on.
    /// The default 51820 is used if this is set to `None`
    pub listen_port: Option<u16>,

    /// The public key of this interface.
    /// If this is `None`, the public key is generated from the private key
    pub public_key: Option<[u8; 32]>,

    /// The private key of this interface
    pub private_key: Option<[u8; 32]>,

    /// The peers that this interface is allowed to communicate with
    pub peers: Vec<SetPeer>,
}

fn encode_name(
    name: &str,
    wireguard: Arc<wireguard_nt_raw::wireguard>,
) -> Result<
    (U16CString, Arc<wireguard_nt_raw::wireguard>),
    (crate::WireGuardError, Arc<wireguard_nt_raw::wireguard>),
> {
    let utf16 = match U16CString::from_str(name) {
        Ok(u) => u,
        Err(e) => return Err((e.into(), wireguard)),
    };
    let max = crate::MAX_NAME;
    if utf16.len() >= max {
        //max_characters is the maximum number of characters including the null terminator. And .len() measures the
        //number of characters (excluding the null terminator). Therefore we can hold a string with
        //max_characters - 1 because the null terminator sits in the last element. A string
        //of length max_characters needs max_characters + 1 to store the null terminator so the >=
        //check holds
        Err((
            format!(
                //TODO: Better error handling
                "Length too large. Size: {}, Max: {}",
                utf16.len(),
                max,
            )
            .into(),
            wireguard,
        ))
    } else {
        Ok((utf16, wireguard))
    }
}

/// Contains information about a single existing adapter
pub struct EnumeratedAdapter {
    /// The name of the adapter
    pub name: String,
}

fn win_error(context: &str, error_code: u32) -> Result<(), Box<dyn std::error::Error>> {
    let e = std::io::Error::from_raw_os_error(error_code as i32);
    Err(format!("{} - {}", context, e).into())
}

const WIREGUARD_STATE_DOWN: i32 = 0;
const WIREGUARD_STATE_UP: i32 = 1;

impl Adapter {
    //TODO: Call get last error for error information on failure and improve error types

    /// Creates a new wireguard adapter inside the pool `pool` with name `name`
    ///
    /// Optionally a GUID can be specified that will become the GUID of this adapter once created.
    pub fn create(
        wireguard: Arc<wireguard_nt_raw::wireguard>,
        pool: &str,
        name: &str,
        guid: Option<u128>,
    ) -> Result<Adapter, (crate::WireGuardError, Arc<wireguard_nt_raw::wireguard>)> {
        let (pool_utf16, wireguard) = encode_name(pool, wireguard)?;
        let (name_utf16, wireguard) = encode_name(name, wireguard)?;

        let guid = match guid {
            Some(guid) => guid,
            None => {
                let mut guid_bytes: [u8; 16] = [0u8; 16];
                rand::thread_rng().fill(&mut guid_bytes);
                u128::from_ne_bytes(guid_bytes)
            }
        };
        //SAFETY: guid is a unique integer so transmuting either all zeroes or the user's preferred
        //guid to the winapi guid type is safe and will allow the windows kernel to see our GUID
        let guid_struct: wireguard_nt_raw::GUID = unsafe { std::mem::transmute(guid) };
        //TODO: The guid of the adapter once created might differ from the one provided because of
        //the byte order of the segments of the GUID struct that are larger than a byte. Verify
        //that this works as expected

        crate::log::set_default_logger_if_unset(&wireguard);

        //SAFETY: the function is loaded from the wireguard dll properly, we are providing valid
        //pointers, and all the strings are correct null terminated UTF-16. This safety rationale
        //applies for all WireGuard* functions below
        let result = unsafe {
            wireguard.WireGuardCreateAdapter(
                pool_utf16.as_ptr(),
                name_utf16.as_ptr(),
                &guid_struct as *const wireguard_nt_raw::GUID,
            )
        };

        if result.is_null() {
            Err(("Failed to crate adapter".into(), wireguard))
        } else {
            Ok(Self {
                adapter: UnsafeHandle(result),
                wireguard,
            })
        }
    }

    /// Attempts to open an existing wireguard with name `name`.
    pub fn open(
        wireguard: Arc<wireguard_nt_raw::wireguard>,
        name: &str,
    ) -> Result<Adapter, (crate::WireGuardError, Arc<wireguard_nt_raw::wireguard>)> {
        let (name_utf16, wireguard) = encode_name(name, wireguard)?;

        crate::log::set_default_logger_if_unset(&wireguard);

        let result = unsafe { wireguard.WireGuardOpenAdapter(name_utf16.as_ptr()) };

        if result.is_null() {
            Err(("WireGuardOpenAdapter failed".into(), wireguard))
        } else {
            Ok(Adapter {
                adapter: UnsafeHandle(result),
                wireguard,
            })
        }
    }

    /// Sets the wireguard configuration of this adapter
    pub fn set_config(&self, config: &SetInterface) -> Result<(), WireGuardError> {
        use std::mem::{align_of, size_of};
        use wireguard_nt_raw::*;

        bitflags::bitflags! {
            struct InterfaceFlags: i32 {
                const HAS_PUBLIC_KEY =  1 << 0;
                const HAS_PRIVATE_KEY = 1 << 1;
                const HAS_LISTEN_PORT = 1 << 2;
                const REPLACE_PEERS =  1 << 3;
            }
        }

        bitflags::bitflags! {
            struct PeerFlags: i32 {
                const HAS_PUBLIC_KEY =  1 << 0;
                const HAS_PRESHARED_KEY = 1 << 1;
                const HAS_PERSISTENT_KEEPALIVE = 1 << 2;
                const HAS_ENDPOINT = 1 << 3;
                const REPLACE_ALLOWED_IPS = 1 << 5;
                const REMOVE = 1 << 6;
                const UPDATE = 1 << 7;
            }
        }

        let peer_size: usize = config
            .peers
            .iter()
            .map(|p| {
                size_of::<WIREGUARD_PEER>()
                    + p.allowed_ips.len() * size_of::<WIREGUARD_ALLOWED_IP>()
            })
            .sum();

        let size: usize = size_of::<WIREGUARD_INTERFACE>() + peer_size;
        let align = align_of::<WIREGUARD_INTERFACE>();

        let mut writer = util::StructWriter::new(size, align);
        //Most of this function is writing data into `writer`, in a format that wireguard expects
        //so that it can decode the data when we call WireGuardSetConfiguration

        // Safety:
        // 1. `writer` has the correct alignment for a `WIREGUARD_INTERFACE`
        // 2. Nothing has been written to writer so the internal pointer must be aligned
        let interface: &mut WIREGUARD_INTERFACE = unsafe { writer.write() };
        interface.Flags = {
            let mut flags = InterfaceFlags::REPLACE_PEERS;
            if let Some(private_key) = &config.private_key {
                flags |= InterfaceFlags::HAS_PRIVATE_KEY;
                interface.PrivateKey.copy_from_slice(private_key);
            }
            if let Some(pub_key) = &config.public_key {
                flags |= InterfaceFlags::HAS_PUBLIC_KEY;
                interface.PublicKey.copy_from_slice(pub_key);
            }

            if let Some(listen_port) = config.listen_port {
                flags |= InterfaceFlags::HAS_LISTEN_PORT;
                interface.ListenPort = listen_port;
            }

            flags.bits
        };
        interface.PeersCount = config.peers.len() as u32;

        for peer in &config.peers {
            // Safety:
            // `align_of::<WIREGUARD_INTERFACE` is 8, WIREGUARD_PEER has no special alignment
            // requirements, and writer is already aligned to hold `WIREGUARD_INTERFACE` structs,
            // therefore we uphold the alignment requirements of `write`
            let mut wg_peer: &mut WIREGUARD_PEER = unsafe { writer.write() };

            wg_peer.Flags = {
                let mut flags = PeerFlags::HAS_ENDPOINT;
                if let Some(pub_key) = &peer.public_key {
                    flags |= PeerFlags::HAS_PUBLIC_KEY;
                    wg_peer.PublicKey.copy_from_slice(pub_key);
                }
                if let Some(preshared_key) = &peer.preshared_key {
                    flags |= PeerFlags::HAS_PRESHARED_KEY;
                    wg_peer.PresharedKey.copy_from_slice(preshared_key);
                }
                if let Some(keep_alive) = peer.keep_alive {
                    flags |= PeerFlags::HAS_PERSISTENT_KEEPALIVE;
                    wg_peer.PersistentKeepalive = keep_alive;
                }
                flags.bits
            };

            log::info!("endpoint: {}", &peer.endpoint);
            match peer.endpoint {
                SocketAddr::V4(v4) => {
                    let addr = unsafe { std::mem::transmute(v4.ip().octets()) };
                    wg_peer.Endpoint.Ipv4.sin_family = winapi::shared::ws2def::AF_INET as u16;
                    //Make sure to put the port in network byte order
                    wg_peer.Endpoint.Ipv4.sin_port = u16::from_ne_bytes(v4.port().to_be_bytes());
                    wg_peer.Endpoint.Ipv4.sin_addr = addr;
                }
                SocketAddr::V6(v6) => {
                    let addr = unsafe { std::mem::transmute(v6.ip().octets()) };
                    wg_peer.Endpoint.Ipv6.sin6_family = winapi::shared::ws2def::AF_INET6 as u16;
                    wg_peer.Endpoint.Ipv4.sin_port = u16::from_ne_bytes(v6.port().to_be_bytes());
                    wg_peer.Endpoint.Ipv6.sin6_addr = addr;
                }
            }

            wg_peer.AllowedIPsCount = peer.allowed_ips.len() as u32;

            for allowed_ip in &peer.allowed_ips {
                // Safety:
                // Same as above, `writer` is aligned because it was aligned before
                let mut wg_allowed_ip: &mut WIREGUARD_ALLOWED_IP = unsafe { writer.write() };
                match allowed_ip {
                    IpNet::V4(v4) => {
                        let addr = unsafe { std::mem::transmute(v4.addr().octets()) };
                        wg_allowed_ip.Address.V4 = addr;
                        wg_allowed_ip.AddressFamily = winapi::shared::ws2def::AF_INET as u16;
                        wg_allowed_ip.Cidr = v4.prefix_len();
                    }
                    IpNet::V6(v6) => {
                        let addr = unsafe { std::mem::transmute(v6.addr().octets()) };
                        wg_allowed_ip.Address.V6 = addr;
                        wg_allowed_ip.AddressFamily = winapi::shared::ws2def::AF_INET6 as u16;
                        wg_allowed_ip.Cidr = v6.prefix_len();
                    }
                }
            }
        }

        //Make sure that our allocation math was correct and that we filled all of writer
        debug_assert!(writer.is_full());

        let result = unsafe {
            self.wireguard.WireGuardSetConfiguration(
                self.adapter.0,
                writer.ptr().cast(),
                size as u32,
            )
        };

        match result {
            0 => Err(format!("WireGuardSetConfiguration failed").into()),
            _ => Ok(()),
        }
    }

    /// Assigns this adapter an ip address and adds route(s) so that packets sent
    /// within the `interface_addr` ipnet will be sent across the WireGuard VPN
    pub fn set_default_route(
        &self,
        interface_addr: Ipv4Net,
        config: &SetInterface,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let luid = self.get_luid();
        unsafe {
            use winapi::shared::netioapi::{
                InitializeUnicastIpAddressEntry, MIB_UNICASTIPADDRESS_ROW,
            };
            use winapi::shared::nldef::IpDadStatePreferred;

            use winapi::shared::netioapi::{CreateIpForwardEntry2, CreateUnicastIpAddressEntry};
            use winapi::shared::winerror::{ERROR_OBJECT_ALREADY_EXISTS, ERROR_SUCCESS};
            use winapi::shared::ws2def::{AF_INET, AF_INET6};

            for allowed_ip in config.peers.iter().map(|p| p.allowed_ips.iter()).flatten() {
                println!("Adding allowed ip: {}", allowed_ip);
                use winapi::shared::netioapi::{InitializeIpForwardEntry, MIB_IPFORWARD_ROW2};
                let mut default_route: MIB_IPFORWARD_ROW2 = std::mem::zeroed();
                InitializeIpForwardEntry(&mut default_route);
                default_route.InterfaceLuid = std::mem::transmute(luid);
                match *allowed_ip {
                    IpNet::V4(v4) => {
                        *default_route.DestinationPrefix.Prefix.si_family_mut() = AF_INET as u16;
                        default_route.DestinationPrefix.Prefix.Ipv4_mut().sin_addr =
                            std::mem::transmute(v4.addr().octets());

                        default_route.DestinationPrefix.PrefixLength = v4.prefix_len();

                        //Next hop is 0.0.0.0/0, because it is the address of a local interface
                        //(the wireguard interface). So because the struct is zeroed we don't need
                        //to set anything except the address family
                        *default_route.NextHop.si_family_mut() = AF_INET as u16;
                    }
                    IpNet::V6(v6) => {
                        *default_route.DestinationPrefix.Prefix.si_family_mut() = AF_INET6 as u16;
                        default_route.DestinationPrefix.Prefix.Ipv6_mut().sin6_addr =
                            std::mem::transmute(v6.addr().octets());

                        default_route.DestinationPrefix.PrefixLength = v6.prefix_len();

                        *default_route.NextHop.si_family_mut() = AF_INET6 as u16;
                    }
                }
                default_route.Metric = 5;

                let err = CreateIpForwardEntry2(&default_route);
                if err != ERROR_SUCCESS && err != ERROR_OBJECT_ALREADY_EXISTS {
                    return win_error("Failed to set default route", err);
                }
            }

            let mut address_row: MIB_UNICASTIPADDRESS_ROW = std::mem::zeroed();
            InitializeUnicastIpAddressEntry(&mut address_row);
            address_row.Address.Ipv4_mut().sin_family = AF_INET as u16;
            address_row.InterfaceLuid = std::mem::transmute(luid);
            address_row.OnLinkPrefixLength = interface_addr.prefix_len();
            address_row.DadState = IpDadStatePreferred;
            address_row.Address.Ipv4_mut().sin_addr =
                std::mem::transmute(interface_addr.addr().octets());

            let err = CreateUnicastIpAddressEntry(&address_row);
            if err != ERROR_SUCCESS && err != ERROR_OBJECT_ALREADY_EXISTS {
                return win_error("Failed to set IP interface", err);
            }

            use winapi::shared::netioapi::{InitializeIpInterfaceEntry, MIB_IPINTERFACE_ROW};
            let mut ip_interface: MIB_IPINTERFACE_ROW = std::mem::zeroed();
            InitializeIpInterfaceEntry(&mut ip_interface);
            ip_interface.InterfaceLuid = std::mem::transmute(luid);
            ip_interface.Family = AF_INET as u16;

            use winapi::shared::netioapi::{GetIpInterfaceEntry, SetIpInterfaceEntry};
            let err = GetIpInterfaceEntry(&mut ip_interface);
            if err != ERROR_SUCCESS {
                return win_error("Failed to get IP interface", err);
            }
            ip_interface.UseAutomaticMetric = 0;
            ip_interface.Metric = 0;
            ip_interface.NlMtu = 1420;
            ip_interface.SitePrefixLength = 0;
            let err = SetIpInterfaceEntry(&mut ip_interface);
            if err != ERROR_SUCCESS {
                return win_error("Failed to set metric and MTU", err);
            }

            Ok(())
        }
    }

    /// Puts this adapter into the up state
    pub fn up(&self) -> bool {
        unsafe {
            self.wireguard
                .WireGuardSetAdapterState(self.adapter.0, WIREGUARD_STATE_UP)
                != 0
        }
    }

    /// Puts this adapter into the down state
    pub fn down(&self) -> bool {
        unsafe {
            self.wireguard
                .WireGuardSetAdapterState(self.adapter.0, WIREGUARD_STATE_DOWN)
                != 0
        }
    }

    /// Returns the adapter's LUID.
    /// This is a 64bit unique identifier that windows uses when referencing this adapter
    pub fn get_luid(&self) -> u64 {
        let mut x = 0u64;
        unsafe {
            self.wireguard
                .WireGuardGetAdapterLUID(self.adapter.0, std::mem::transmute(&mut x))
        };
        x
    }

    /// Sets the logging level of this adapter
    ///
    /// Log messages will be sent to the current logger (set using [`crate::set_logger`]
    pub fn set_logging(&self, level: AdapterLoggingLevel) -> bool {
        let level = match level {
            AdapterLoggingLevel::Off => 0,
            AdapterLoggingLevel::On => 1,
            AdapterLoggingLevel::OnWithPrefix => 2,
        };
        unsafe {
            self.wireguard
                .WireGuardSetAdapterLogging(self.adapter.0, level)
                != 0
        }
    }
}

impl Drop for Adapter {
    fn drop(&mut self) {
        //Free adapter on drop
        //This is why we need an Arc of wireguard, so we have access to it here
        unsafe { self.wireguard.WireGuardCloseAdapter(self.adapter.0) };
        self.adapter = UnsafeHandle(ptr::null_mut());
    }
}
