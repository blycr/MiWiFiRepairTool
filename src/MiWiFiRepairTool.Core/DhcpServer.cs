using System.Net;
using System.Net.Sockets;

namespace MiWiFiRepairTool.Core;

/// <summary>
/// Minimal RFC 2131 DHCP server tailored to Xiaomi router TFTP recovery.
/// Hands out addresses from a fixed pool, keeps a MAC -> IP lease table,
/// answers DISCOVER/OFFER, REQUEST/ACK(/NAK), RELEASE and DECLINE, plus INFORM.
/// Log lines intentionally resemble the original tool's tftp.log output.
/// </summary>
public sealed class DhcpServer : IDisposable
{
    public const int ServerPort = 67;
    public const int ClientPort = 68;

    private readonly IPAddress _serverIp;
    private readonly IPAddress _poolStart;
    private readonly int _poolSize;
    private readonly IPAddress _mask;
    private readonly long _leaseSeconds;
    private readonly IPAddress _bindAddress; // IPAddress.Any in production, loopback in selftest

    private Socket? _socket;
    private Thread? _thread;
    private volatile bool _running;

    private readonly object _lock = new();
    private readonly Dictionary<string, Lease> _leases = new(StringComparer.OrdinalIgnoreCase);
    private readonly HashSet<long> _badIps = new(); // declined addresses

    /// <summary>Log output (thread-safe events).</summary>
    public event Action<string>? Log;

    /// <summary>Raised when a lease is assigned or acked, for the GUI.</summary>
    public event Action<string, IPAddress>? LeaseChanged;

    public bool IsRunning => _running;

    private sealed class Lease
    {
        public IPAddress Ip = IPAddress.Any;
        public DateTime ExpiresUtc;
        public bool Acked;
    }

    public DhcpServer(
        IPAddress serverIp,
        IPAddress poolStart,
        int poolSize = 100,
        IPAddress? mask = null,
        long leaseSeconds = 172800,
        IPAddress? bindAddress = null)
    {
        _serverIp = serverIp;
        _poolStart = poolStart;
        _poolSize = Math.Max(1, poolSize);
        _mask = mask ?? IPAddress.Parse("255.255.255.0");
        _leaseSeconds = leaseSeconds;
        _bindAddress = bindAddress ?? IPAddress.Any;
    }

    public IPAddress ServerIp => _serverIp;

    // ------------------------------------------------------------------ lifecycle

    public void Start()
    {
        if (_running) return;
        _socket = new Socket(AddressFamily.InterNetwork, SocketType.Dgram, ProtocolType.Udp);
        _socket.SetSocketOption(SocketOptionLevel.Socket, SocketOptionName.ReuseAddress, true);
        _socket.SetSocketOption(SocketOptionLevel.Socket, SocketOptionName.Broadcast, true);
        _socket.Bind(new IPEndPoint(_bindAddress, ServerPort));
        _running = true;
        _thread = new Thread(Loop) { IsBackground = true, Name = "dhcp-server" };
        _thread.Start();
        Log?.Invoke($"DHCP server started on {_serverIp}, pool {_poolStart} - {PoolEnd()}, lease {_leaseSeconds}s");
    }

    public void Stop()
    {
        if (!_running) return;
        _running = false;
        try { _socket?.Close(); } catch { /* ignore */ }
        _socket = null;
        if (_thread is { IsAlive: true })
        {
            if (!_thread.Join(2000)) _thread.Interrupt();
        }
        _thread = null;
        Log?.Invoke("DHCP server stopped");
    }

    public void Dispose() => Stop();

    private IPAddress PoolEnd()
    {
        long end = IpToLong(_poolStart) + _poolSize - 1;
        return LongToIp(end);
    }

    // ------------------------------------------------------------------ receive loop

    private void Loop()
    {
        var buffer = new byte[2048];
        EndPoint remote = new IPEndPoint(IPAddress.Any, 0);
        while (_running)
        {
            try
            {
                _socket!.ReceiveTimeout = 250;
                int n = _socket.ReceiveFrom(buffer, ref remote);
                if (n >= 240 && _running)
                {
                    var copy = new byte[n];
                    Array.Copy(buffer, copy, n);
                    Handle(copy, (IPEndPoint)remote);
                }
            }
            catch (SocketException)
            {
                // receive timeout or closed socket -> loop again / exit
            }
            catch (ObjectDisposedException)
            {
                break;
            }
        }
    }

    // ------------------------------------------------------------------ message handling

    private void Handle(byte[] pkt, IPEndPoint src)
    {
        if (pkt[0] != 1) return; // BOOTREQUEST only
        if (pkt[1] != 1) return; // Ethernet hardware
        byte hlen = pkt[2];
        if (hlen < 6) return;

        uint xid = Be32(pkt, 4);
        ushort flags = Be16(pkt, 10);
        uint ciaddr = Be32(pkt, 12);
        uint giaddr = Be32(pkt, 24);
        var chaddr = new byte[16];
        Array.Copy(pkt, 28, chaddr, 0, 16);
        string mac = FormatMac(chaddr, hlen);

        if (Be32(pkt, 236) != 0x63825363) return; // DHCP magic cookie
        var options = ParseOptions(pkt, 240);
        if (!options.TryGetValue(53, out var typeOpt) || typeOpt.Length == 0) return;
        int msgType = typeOpt[0];

        switch (msgType)
        {
            case 1: // DHCPDISCOVER
                Log?.Invoke($"Rcvd DHCP Discover Msg for IP {IpToString(ciaddr)}, Mac {mac}");
                HandleDiscover(xid, flags, mac, chaddr, giaddr, src);
                break;
            case 3: // DHCPREQUEST
                Log?.Invoke($"Rcvd DHCP Rqst Msg for IP {IpToString(ciaddr)}, Mac {mac}");
                HandleRequest(xid, flags, mac, chaddr, giaddr, ciaddr, options, src);
                break;
            case 7: // DHCPRELEASE
                Log?.Invoke($"Rcvd DHCP release Msg for IP {IpToString(ciaddr)}, Mac {mac}");
                HandleRelease(mac, ciaddr);
                break;
            case 4: // DHCPDECLINE
                Log?.Invoke("DHCP decline");
                if (options.TryGetValue(50, out var req) && req.Length == 4)
                    lock (_lock) { _badIps.Add(Be32(req, 0)); }
                break;
            case 8: // DHCPINFORM
                Log?.Invoke("DHCP inform");
                SendReply(xid, flags, chaddr, giaddr, IPAddress.Any, 2, isInform: true, src);
                break;
        }
    }

    private void HandleDiscover(uint xid, ushort flags, string mac, byte[] chaddr, uint giaddr, IPEndPoint src)
    {
        lock (_lock)
        {
            if (_leases.TryGetValue(mac, out var existing) &&
                existing.ExpiresUtc > DateTime.UtcNow &&
                !_badIps.Contains(IpToLong(existing.Ip)))
            {
                // keep the previously allocated address (lease persistence)
                Log?.Invoke($"DHCP: proposed address {existing.Ip}");
                existing.Acked = false;
                existing.ExpiresUtc = DateTime.UtcNow.AddMinutes(1); // provisional until REQUEST
                LeaseChanged?.Invoke(mac, existing.Ip);
                SendReply(xid, flags, chaddr, giaddr, existing.Ip, 2, false, src);
                return;
            }

            IPAddress? ip = AllocateFree(mac);
            if (ip is null)
            {
                Log?.Invoke("DHCP Pool is empty");
                return;
            }
            Log?.Invoke($"DHCP: proposed address {ip}");
            LeaseChanged?.Invoke(mac, ip);
            SendReply(xid, flags, chaddr, giaddr, ip, 2, false, src);
        }
    }

    private void HandleRequest(uint xid, ushort flags, string mac, byte[] chaddr, uint giaddr, uint ciaddr, Dictionary<int, byte[]> options, IPEndPoint src)
    {
        // if the client names a different DHCP server, stay silent (RFC 2131 4.4.1)
        if (options.TryGetValue(54, out var sid) && sid.Length == 4 && Be32(sid, 0) != IpToLong(_serverIp))
        {
            Log?.Invoke("DHCP Double Answer : ignored");
            return;
        }

        lock (_lock)
        {
            // RENEW / REBIND state
            if (ciaddr != 0)
            {
                IPAddress ip = LongToIp(ciaddr);
                if (_leases.TryGetValue(mac, out var l) && l.Ip.Equals(ip))
                {
                    l.Acked = true;
                    l.ExpiresUtc = DateTime.UtcNow.AddSeconds(_leaseSeconds);
                    Log?.Invoke($"Previously allocated address {ip} acked");
                    LeaseChanged?.Invoke(mac, ip);
                    SendReply(xid, flags, chaddr, giaddr, ip, 5, false, src);
                }
                else
                {
                    Log?.Invoke("DHCP Nak");
                    SendNak(xid, flags, chaddr, giaddr, src);
                }
                return;
            }

            IPAddress? requested = options.TryGetValue(50, out var req) && req.Length == 4
                ? LongToIp(Be32(req, 0))
                : null;

            // SELECTING state: we offered this address
            if (requested is not null && IsInPool(requested) && !_badIps.Contains(IpToLong(requested)))
            {
                bool ours = _leases.TryGetValue(mac, out var l) && l.Ip.Equals(requested);
                bool free = !_leases.Values.Any(v => v.Ip.Equals(requested));
                if (ours || free)
                {
                    var lease = _leases.TryGetValue(mac, out var l2) ? l2 : new Lease();
                    lease.Ip = requested;
                    lease.Acked = true;
                    lease.ExpiresUtc = DateTime.UtcNow.AddSeconds(_leaseSeconds);
                    _leases[mac] = lease;
                    Log?.Invoke($"Previously allocated address {requested} acked");
                    LeaseChanged?.Invoke(mac, requested);
                    SendReply(xid, flags, chaddr, giaddr, requested, 5, false, src);
                    return;
                }
            }

            // fall back to the MAC's existing lease if any
            if (_leases.TryGetValue(mac, out var existing))
            {
                existing.Acked = true;
                existing.ExpiresUtc = DateTime.UtcNow.AddSeconds(_leaseSeconds);
                Log?.Invoke($"Previously allocated address {existing.Ip} acked");
                LeaseChanged?.Invoke(mac, existing.Ip);
                SendReply(xid, flags, chaddr, giaddr, existing.Ip, 5, false, src);
                return;
            }

            Log?.Invoke("DHCP Nak");
            SendNak(xid, flags, chaddr, giaddr, src);
        }
    }

    private void HandleRelease(string mac, uint ciaddr)
    {
        lock (_lock)
        {
            if (_leases.TryGetValue(mac, out _))
            {
                _leases.Remove(mac);
                Log?.Invoke($"item {mac} released");
            }
            if (ciaddr != 0) _badIps.Remove(ciaddr); // released address is reusable
        }
    }

    private IPAddress? AllocateFree(string mac)
    {
        long start = IpToLong(_poolStart);
        for (int i = 0; i < _poolSize; i++)
        {
            long raw = start + i;
            if (raw == IpToLong(_serverIp)) continue;
            if (_badIps.Contains(raw)) continue;
            var ip = LongToIp(raw);
            var holder = _leases.FirstOrDefault(kv => kv.Value.Ip.Equals(ip));
            if (holder.Key is not null && !string.Equals(mac, holder.Key, StringComparison.OrdinalIgnoreCase))
                continue;
            var lease = _leases.TryGetValue(mac, out var l) ? l : new Lease();
            lease.Ip = ip;
            lease.Acked = false;
            lease.ExpiresUtc = DateTime.UtcNow.AddMinutes(1);
            _leases[mac] = lease;
            return ip;
        }
        return null;
    }

    // ------------------------------------------------------------------ reply building

    private void SendReply(uint xid, ushort flags, byte[] chaddr, uint giaddr, IPAddress yiaddr,
        byte msgType, bool isInform, IPEndPoint src)
    {
        var pkt = BuildReply(xid, flags, chaddr, giaddr, yiaddr, msgType, isInform);
        SendTo(ReplyDestination(src, giaddr, flags), pkt);
    }

    private void SendNak(uint xid, ushort flags, byte[] chaddr, uint giaddr, IPEndPoint src)
    {
        var pkt = BuildReply(xid, flags, chaddr, giaddr, IPAddress.Any, 6, false);
        SendTo(ReplyDestination(src, giaddr, flags), pkt);
    }

    private static EndPoint ReplyDestination(IPEndPoint src, uint giaddr, ushort flags)
    {
        if (giaddr != 0) return new IPEndPoint(LongToIp(giaddr), ServerPort); // BOOTP relay
        bool fromZero = src.Address.Equals(IPAddress.Any);
        // broadcast when the client asked for it or spoke from 0.0.0.0 (recovery mode)
        if ((flags & 0x8000) != 0 || fromZero)
            return new IPEndPoint(IPAddress.Broadcast, ClientPort);
        // otherwise unicast straight back to whoever asked (also used by the selftest)
        return new IPEndPoint(src.Address, ClientPort);
    }

    private void SendTo(EndPoint dest, byte[] pkt)
    {
        try
        {
            _socket?.SendTo(pkt, dest);
        }
        catch (SocketException)
        {
            // link down / no route: ignore
        }
    }

    private byte[] BuildReply(uint xid, ushort flags, byte[] chaddr, uint giaddr, IPAddress yiaddr, byte msgType, bool isInform)
    {
        var b = new byte[300]; // DHCP minimum size
        b[0] = 2;              // BOOTREPLY
        b[1] = 1;              // Ethernet
        b[2] = 6;              // MAC length
        PutBe32(b, 4, xid);
        PutBe16(b, 10, flags);
        if (isInform) PutBe32(b, 12, (uint)IpToLong(yiaddr));      // ciaddr echo
        else PutBe32(b, 16, (uint)IpToLong(yiaddr));               // yiaddr
        PutBe32(b, 20, (uint)IpToLong(_serverIp));                 // siaddr = bootstrap server
        PutBe32(b, 24, giaddr);
        Array.Copy(chaddr, 0, b, 28, 16);
        PutBe32(b, 236, 0x63825363);

        int o = 240;
        o = AddOpt(b, o, 53, new byte[] { msgType });
        o = AddOpt(b, o, 54, ToBytes((uint)IpToLong(_serverIp)));
        if (!isInform)
        {
            o = AddOpt(b, o, 51, ToBytes((uint)_leaseSeconds));
            o = AddOpt(b, o, 58, ToBytes((uint)(_leaseSeconds * 0.5)));
            o = AddOpt(b, o, 59, ToBytes((uint)(_leaseSeconds * 0.875)));
            o = AddOpt(b, o, 1, ToBytes((uint)IpToLong(_mask)));
            o = AddOpt(b, o, 3, ToBytes((uint)IpToLong(_serverIp)));
            o = AddOpt(b, o, 6, ToBytes((uint)IpToLong(_serverIp)));
        }
        b[o++] = 0xFF; // end
        return b;
    }

    private static int AddOpt(byte[] b, int o, int code, byte[] val)
    {
        b[o++] = (byte)code;
        b[o++] = (byte)val.Length;
        Array.Copy(val, 0, b, o, val.Length);
        return o + val.Length;
    }

    private static Dictionary<int, byte[]> ParseOptions(byte[] pkt, int start)
    {
        var map = new Dictionary<int, byte[]>();
        int i = start;
        while (i < pkt.Length)
        {
            byte code = pkt[i];
            if (code == 0) { i++; continue; }   // pad
            if (code == 255) break;             // end
            if (i + 1 >= pkt.Length) break;
            int len = pkt[i + 1];
            if (i + 2 + len > pkt.Length) break;
            var val = new byte[len];
            Array.Copy(pkt, i + 2, val, 0, len);
            map[code] = val;
            i += 2 + len;
        }
        return map;
    }

    // ------------------------------------------------------------------ helpers

    private bool IsInPool(IPAddress ip)
    {
        long raw = IpToLong(ip);
        return raw >= IpToLong(_poolStart) && raw < IpToLong(_poolStart) + _poolSize;
    }

    private static string FormatMac(byte[] chaddr, byte len)
    {
        var sb = new System.Text.StringBuilder();
        for (int i = 0; i < len; i++)
        {
            if (i > 0) sb.Append(':');
            sb.Append(chaddr[i].ToString("X2"));
        }
        return sb.ToString();
    }

    private static string IpToString(uint raw) => raw == 0 ? "0.0.0.0" : new IPAddress(raw).ToString();

    private static uint Be32(byte[] b, int o) => (uint)((b[o] << 24) | (b[o + 1] << 16) | (b[o + 2] << 8) | b[o + 3]);
    private static ushort Be16(byte[] b, int o) => (ushort)((b[o] << 8) | b[o + 1]);
    private static void PutBe32(byte[] b, int o, uint v)
    {
        b[o] = (byte)(v >> 24); b[o + 1] = (byte)(v >> 16); b[o + 2] = (byte)(v >> 8); b[o + 3] = (byte)v;
    }
    private static void PutBe16(byte[] b, int o, ushort v)
    {
        b[o] = (byte)(v >> 8); b[o + 1] = (byte)v;
    }

    private static long IpToLong(IPAddress ip)
    {
        uint raw = BitConverter.ToUInt32(ip.GetAddressBytes(), 0);
        return (raw >> 24) | ((raw >> 8) & 0x0000FF00) | ((raw << 8) & 0x00FF0000) | (raw << 24);
    }

    private static IPAddress LongToIp(long raw)
    {
        uint v = (uint)raw;
        uint n = (v >> 24) | ((v >> 8) & 0x0000FF00) | ((v << 8) & 0x00FF0000) | (v << 24);
        return new IPAddress(BitConverter.GetBytes(n));
    }

    private static byte[] ToBytes(uint v) => new[] { (byte)(v >> 24), (byte)(v >> 16), (byte)(v >> 8), (byte)v };
}
