using System.Diagnostics;
using System.Net;
using System.Net.Sockets;
using System.Text;

namespace MiWiFiRepairTool.Core;

/// <summary>
/// TFTP server (RFC 1350 + RFC 2347/2348/2349 options) used to feed a firmware
/// image to a Xiaomi router in recovery mode. Serves files from a single root
/// directory; supports RRQ and WRQ, blksize/tsize/timeout negotiation via OACK,
/// retransmission and per-transfer statistics. Log lines resemble the original
/// tool's tftp.log output.
/// </summary>
public sealed class TftpServer : IDisposable
{
    public const int ServerPort = 69;

    private readonly string _rootDir;
    private readonly int _defaultTimeoutSeconds;
    private readonly int _maxRetries;
    private readonly IPAddress _bindAddress; // Any in production, loopback in selftest

    private Socket? _socket;
    private Thread? _thread;
    private volatile bool _running;

    private readonly object _transfersLock = new();
    private readonly HashSet<Socket> _transferSockets = new();

    public event Action<string>? Log;

    /// <summary>Progress / completion of a transfer (thread-safe).</summary>
    public event Action<TransferInfo>? TransferUpdated;

    public string RootDir => _rootDir;
    public bool IsRunning => _running;

    public sealed class TransferInfo
    {
        public required string FileName;
        public required string Remote;
        public long BytesSent;
        public long TotalBytes;   // -1 for uploads
        public int Blocks;        // DATA packets sent (RRQ) or received (WRQ)
        public int Retransmits;
        public bool Done;
        public bool IsUpload;
        public string? Error;
        public double Seconds;
    }

    public TftpServer(string rootDir, int timeoutSeconds = 5, int maxRetries = 5, IPAddress? bindAddress = null)
    {
        _rootDir = Path.GetFullPath(rootDir);
        _defaultTimeoutSeconds = Math.Clamp(timeoutSeconds, 1, 255);
        _maxRetries = Math.Max(1, maxRetries);
        _bindAddress = bindAddress ?? IPAddress.Any;
    }

    // ------------------------------------------------------------------ lifecycle

    public void Start()
    {
        if (_running) return;
        _socket = new Socket(AddressFamily.InterNetwork, SocketType.Dgram, ProtocolType.Udp);
        _socket.SetSocketOption(SocketOptionLevel.Socket, SocketOptionName.ReuseAddress, true);
        _socket.Bind(new IPEndPoint(_bindAddress, ServerPort));
        _running = true;
        _thread = new Thread(Loop) { IsBackground = true, Name = "tftp-server" };
        _thread.Start();
        Log?.Invoke($"TFTP server started on port {ServerPort}, root {_rootDir}");
    }

    public void Stop()
    {
        if (!_running) return;
        _running = false;
        try { _socket?.Close(); } catch { /* ignore */ }
        _socket = null;
        lock (_transfersLock)
        {
            foreach (var s in _transferSockets) { try { s.Close(); } catch { /* ignore */ } }
            _transferSockets.Clear();
        }
        if (_thread is { IsAlive: true })
        {
            if (!_thread.Join(2000)) _thread.Interrupt();
        }
        _thread = null;
        Log?.Invoke("TFTP server stopped");
    }

    public void Dispose() => Stop();

    // ------------------------------------------------------------------ main receive loop

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
                if (n < 2 || !_running) continue;
                ushort op = (ushort)((buffer[0] << 8) | buffer[1]);
                if (op is 1 or 2) // RRQ / WRQ
                {
                    var pkt = new byte[n];
                    Array.Copy(buffer, pkt, n);
                    var client = (IPEndPoint)remote;
                    ThreadPool.QueueUserWorkItem(_ => SafeHandleTransfer(client, op, pkt));
                }
            }
            catch (SocketException)
            {
                // receive timeout or closed socket
            }
            catch (ObjectDisposedException)
            {
                break;
            }
        }
    }

    private void SafeHandleTransfer(IPEndPoint client, ushort op, byte[] pkt)
    {
        try
        {
            HandleTransfer(client, op, pkt);
        }
        catch (Exception ex)
        {
            Log?.Invoke($"TFTP transfer error: {ex.Message}");
        }
    }

    // ------------------------------------------------------------------ transfer handling

    private void HandleTransfer(IPEndPoint client, ushort op, byte[] pkt)
    {
        // parse filename / mode / options
        int pos = 2;
        string fileName = ReadCString(pkt, ref pos, Encoding.UTF8);
        string mode = ReadCString(pkt, ref pos, Encoding.ASCII);
        var options = new Dictionary<string, string>(StringComparer.OrdinalIgnoreCase);
        while (pos < pkt.Length)
        {
            string name = ReadCString(pkt, ref pos, Encoding.ASCII);
            if (name.Length == 0) break;
            string value = ReadCString(pkt, ref pos, Encoding.ASCII);
            options[name] = value;
        }

        string safe = SafeName(fileName);
        string path = Path.Combine(_rootDir, safe);

        if (op == 1 && !File.Exists(path))
        {
            using var tmp = MakeSocket(out _);
            SendError(tmp, client, 1, "File not found");
            Log?.Invoke($"Read request for file <{fileName}>. Mode {mode} -> not found");
            return;
        }

        Socket ts = MakeSocket(out int localPort);
        lock (_transfersLock) _transferSockets.Add(ts);
        var info = new TransferInfo
        {
            FileName = fileName,
            Remote = client.ToString(),
            TotalBytes = op == 1 ? new FileInfo(path).Length : -1,
            IsUpload = op == 2,
        };
        try
        {
            Log?.Invoke($"Connection received from {client.Address} on port {client.Port}");
            Log?.Invoke($"{(op == 1 ? "Read" : "Write")} request for file <{fileName}>. Mode {mode}");
            Log?.Invoke($"Using local port {localPort}");

            int blksize = 512;
            int timeout = _defaultTimeoutSeconds;
            if (options.TryGetValue("blksize", out var bs) && int.TryParse(bs, out int bsv))
                blksize = Math.Clamp(bsv, 8, 65464);
            if (options.TryGetValue("timeout", out var to) && int.TryParse(to, out int tov))
                timeout = Math.Clamp(tov, 1, 255);
            ts.ReceiveTimeout = timeout * 1000;

            bool wantOack = options.Count > 0;
            byte[]? oackPkt = null;
            if (wantOack)
            {
                var oack = new List<(string, string)>
                {
                    ("blksize", blksize.ToString()),
                    ("timeout", timeout.ToString()),
                };
                if (op == 1) oack.Add(("tsize", info.TotalBytes.ToString()));
                oackPkt = BuildOack(oack);
                ts.SendTo(oackPkt, client);
                Log?.Invoke("OACK: <" + string.Join(",", oack.Select(x => $"{x.Item1}={x.Item2}")) + ",>");
            }

            if (op == 1) Rrq(ts, client, path, blksize, info, oackPkt);
            else Wrq(ts, client, path, blksize, info, wantOack);
        }
        finally
        {
            try { ts.Close(); } catch { /* ignore */ }
            lock (_transfersLock) _transferSockets.Remove(ts);
        }
    }

    private Socket MakeSocket(out int localPort)
    {
        var s = new Socket(AddressFamily.InterNetwork, SocketType.Dgram, ProtocolType.Udp);
        s.SetSocketOption(SocketOptionLevel.Socket, SocketOptionName.ReuseAddress, true);
        s.Bind(new IPEndPoint(_bindAddress, 0));
        localPort = ((IPEndPoint)s.LocalEndPoint!).Port;
        return s;
    }

    private void Rrq(Socket ts, IPEndPoint client, string path, int blksize, TransferInfo info, byte[]? oackPkt)
    {
        using var fs = File.OpenRead(path);
        var data = new byte[blksize];
        var buf = new byte[2048];
        int block = 1;
        long bytes = 0;
        int retr = 0;
        var sw = Stopwatch.StartNew();

        // after OACK the client first acknowledges block 0 (resend the OACK on timeout)
        if (oackPkt is not null &&
            !WaitAck(ts, 0, ref retr, _maxRetries, buf, onResend: () => ts.SendTo(oackPkt, client)))
        {
            info.Error = "timeout waiting for initial ACK";
            Finish(info, sw, bytes, 0, retr);
            return;
        }

        bool last = false;
        while (!last)
        {
            int n = fs.Read(data, 0, blksize);
            SendData(ts, client, block, data, n);
            info.Blocks = block;
            bool acked = WaitAck(ts, block, ref retr, _maxRetries, buf, onResend: () => SendData(ts, client, block, data, n));
            if (!acked)
            {
                info.Error = "timeout";
                Finish(info, sw, bytes, block, retr);
                return;
            }
            bytes += n;
            info.BytesSent = bytes;
            if (n < blksize) last = true;
            block++;
            FireProgress(info, sw);
        }
        Finish(info, sw, bytes, block - 1, retr);
        Log?.Invoke($"<{info.FileName}>: sent {info.Blocks} blks, {bytes} bytes in {sw.Elapsed.TotalSeconds:F0} s. {retr} blk resent");
    }

    private void Wrq(Socket ts, IPEndPoint client, string path, int blksize, TransferInfo info, bool oackSent)
    {
        using var fs = File.Create(path);
        var buf = new byte[65572];
        int expected = 1;
        ushort lastAcked = 0;
        long bytes = 0;
        int retr = 0;
        var sw = Stopwatch.StartNew();

        if (!oackSent)
        {
            SendAck(ts, client, 0);
            lastAcked = 0;
        }

        int timeouts = 0;
        while (true)
        {
            int rn;
            try { rn = ts.Receive(buf); }
            catch (SocketException) { rn = -1; }

            if (rn < 0)
            {
                retr++;
                if (++timeouts > _maxRetries)
                {
                    info.Error = "timeout";
                    break;
                }
                SendAck(ts, client, lastAcked);
                continue;
            }
            timeouts = 0;

            ushort op = (ushort)((buf[0] << 8) | buf[1]);
            if (op == 3) // DATA
            {
                ushort blk = (ushort)((buf[2] << 8) | buf[3]);
                if (blk == expected)
                {
                    int payload = rn - 4;
                    fs.Write(buf, 4, payload);
                    bytes += payload;
                    info.BytesSent = bytes;
                    info.Blocks = expected;
                    SendAck(ts, client, blk);
                    lastAcked = blk;
                    if (payload < blksize) break; // final block
                    expected++;
                }
                else if (blk < expected)
                {
                    // duplicate DATA: re-acknowledge the last block
                    SendAck(ts, client, lastAcked);
                }
                // blk > expected: out-of-order, ignore
            }
            else if (op == 5) // ERROR
            {
                info.Error = "client aborted";
                break;
            }
            FireProgress(info, sw);
        }

        info.Done = true;
        info.Seconds = sw.Elapsed.TotalSeconds;
        info.Retransmits = retr;
        info.BytesSent = bytes;
        info.Blocks = expected; // final block counted; expected not yet incremented past it
        TransferUpdated?.Invoke(info);
        Log?.Invoke($"<{info.FileName}>: received {info.Blocks} blks, {bytes} bytes in {sw.Elapsed.TotalSeconds:F0} s. {retr} blk resent");
    }

    private void Finish(TransferInfo info, Stopwatch sw, long bytes, int blocks, int retr)
    {
        info.Done = true;
        info.Seconds = sw.Elapsed.TotalSeconds;
        info.Retransmits = retr;
        info.BytesSent = bytes;
        info.Blocks = Math.Max(0, blocks);
        TransferUpdated?.Invoke(info);
    }

    private void FireProgress(TransferInfo info, Stopwatch sw)
    {
        info.Seconds = sw.Elapsed.TotalSeconds;
        TransferUpdated?.Invoke(info);
    }

    /// <summary>Waits for an ACK of <paramref name="expected"/>; resends via <paramref name="onResend"/> on timeout/dup.</summary>
    private bool WaitAck(Socket ts, int expected, ref int retr, int maxRetries, byte[] buf, Action onResend)
    {
        int timeouts = 0;
        while (true)
        {
            int rn;
            try { rn = ts.Receive(buf); }
            catch (SocketException) { rn = -1; }

            if (rn < 0)
            {
                retr++;
                if (++timeouts > maxRetries) return false;
                onResend();
                continue;
            }
            timeouts = 0;

            ushort op = (ushort)((buf[0] << 8) | buf[1]);
            if (op == 4) // ACK
            {
                ushort blk = (ushort)((buf[2] << 8) | buf[3]);
                if (blk == expected) return true;
                if (blk < expected)
                {
                    // duplicate ACK of an earlier block: current block was lost
                    retr++;
                    onResend();
                }
                // blk > expected: out of order, keep waiting
            }
            else if (op == 5)
            {
                return false; // ERROR from client
            }
        }
    }

    // ------------------------------------------------------------------ packet builders

    private static void SendData(Socket ts, IPEndPoint client, int block, byte[] data, int len)
    {
        var pkt = new byte[4 + len];
        pkt[1] = 3;
        pkt[2] = (byte)(block >> 8);
        pkt[3] = (byte)block;
        Array.Copy(data, 0, pkt, 4, len);
        ts.SendTo(pkt, client);
    }

    private static void SendAck(Socket ts, IPEndPoint client, int block)
    {
        var pkt = new byte[4];
        pkt[1] = 4;
        pkt[2] = (byte)(block >> 8);
        pkt[3] = (byte)block;
        ts.SendTo(pkt, client);
    }

    private static void BuildOackBytes(MemoryStream ms, List<(string, string)> options)
    {
        ms.WriteByte(0); ms.WriteByte(6);
        foreach (var (k, v) in options)
        {
            var kb = Encoding.ASCII.GetBytes(k);
            var vb = Encoding.ASCII.GetBytes(v);
            ms.Write(kb); ms.WriteByte(0); ms.Write(vb); ms.WriteByte(0);
        }
    }

    private static byte[] BuildOack(List<(string, string)> options)
    {
        using var ms = new MemoryStream();
        BuildOackBytes(ms, options);
        return ms.ToArray();
    }

    private static void SendError(Socket ts, IPEndPoint client, int code, string message)
    {
        var body = Encoding.ASCII.GetBytes(message + "\0");
        var pkt = new byte[4 + body.Length];
        pkt[1] = 5;
        pkt[2] = (byte)(code >> 8);
        pkt[3] = (byte)code;
        Array.Copy(body, 0, pkt, 4, body.Length);
        ts.SendTo(pkt, client);
    }

    private static string ReadCString(byte[] pkt, ref int pos, Encoding enc)
    {
        if (pos >= pkt.Length) return string.Empty;
        int end = Array.IndexOf(pkt, (byte)0, pos);
        if (end == -1) { pos = pkt.Length; return string.Empty; }
        string s = enc.GetString(pkt, pos, end - pos);
        pos = end + 1;
        return s;
    }

    /// <summary>Basename only, so a malicious name cannot escape the root directory.</summary>
    private static string SafeName(string fileName)
    {
        string name = fileName.Replace('\\', '/');
        int slash = name.LastIndexOf('/');
        return slash >= 0 ? name[(slash + 1)..] : name;
    }
}
