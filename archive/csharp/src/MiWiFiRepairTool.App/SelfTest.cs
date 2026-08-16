using System.Net;
using System.Net.Sockets;
using System.Text;
using MiWiFiRepairTool.Core;

namespace MiWiFiRepairTool.App;

/// <summary>
/// Headless self-test: runs the DHCP and TFTP servers on the loopback
/// interface and exercises them with synthetic clients. Invoked with
/// <c>--selftest</c>; exit code 0 = all pass.
/// </summary>
internal static class SelfTest
{
    public static int Run()
    {
        Console.WriteLine("=== MiWiFiRepairTool self test ===");
        int fails = 0;
        fails += TestDhcp() ? 0 : 1;
        fails += TestTftpRrq() ? 0 : 1;
        fails += TestTftpWrq() ? 0 : 1;
        fails += TestTftpNotFound() ? 0 : 1;
        fails += TestRomParse() ? 0 : 1;
        TestRomLive();
        Console.WriteLine(fails == 0 ? "SELFTEST: ALL PASS" : $"SELFTEST: {fails} FAILED");
        return fails == 0 ? 0 : 1;
    }

    // ================================================================ DHCP

    private static bool TestDhcp()
    {
        Console.Write("DHCP discover/offer/request/ack/nak ... ");
        var mac1 = new byte[] { 0x04, 0x67, 0x61, 0x9A, 0x3A, 0x3C };
        var mac2 = new byte[] { 0x04, 0x67, 0x61, 0x9A, 0x3A, 0x3D };
        var server = new DhcpServer(IPAddress.Parse("192.168.31.1"), IPAddress.Parse("192.168.31.100"), 100,
            bindAddress: IPAddress.Loopback);
        server.Log += _ => { };
        server.Start();
        try
        {
            using var cli = new Socket(AddressFamily.InterNetwork, SocketType.Dgram, ProtocolType.Udp);
            cli.Bind(new IPEndPoint(IPAddress.Loopback, 68));
            cli.ReceiveTimeout = 3000;
            var srv = new IPEndPoint(IPAddress.Loopback, DhcpServer.ServerPort);

            // --- DISCOVER -> OFFER
            cli.SendTo(BuildDhcp(0x12345678, mac1, 1), srv);
            var offer = RecvDhcp(cli);
            if (offer is null) { Console.WriteLine("FAIL (no OFFER)"); return false; }
            uint yiaddr = Be32(offer, 16);
            if (yiaddr < 0xC0A81F64 || yiaddr > 0xC0A81FC7) { Console.WriteLine($"FAIL (yiaddr 0x{yiaddr:X8})"); return false; }
            if (GetOpt(offer, 53) is not { } t1 || t1[0] != 2) { Console.WriteLine("FAIL (option 53 != OFFER)"); return false; }
            if (GetOpt(offer, 54) is not { } sid || Be32(sid, 0) != 0xC0A81F01) { Console.WriteLine("FAIL (server id)"); return false; }
            if (GetOpt(offer, 1) is not { } mask || Be32(mask, 0) != 0xFFFFFF00) { Console.WriteLine("FAIL (subnet mask)"); return false; }
            if (GetOpt(offer, 3) is not { } gw || Be32(gw, 0) != 0xC0A81F01) { Console.WriteLine("FAIL (router option)"); return false; }

            // --- REQUEST -> ACK
            cli.SendTo(BuildDhcp(0x12345678, mac1, 3, requested: yiaddr, serverId: 0xC0A81F01), srv);
            var ack = RecvDhcp(cli);
            if (ack is null) { Console.WriteLine("FAIL (no ACK)"); return false; }
            if (GetOpt(ack, 53) is not { } t2 || t2[0] != 5) { Console.WriteLine("FAIL (option 53 != ACK)"); return false; }
            if (Be32(ack, 16) != yiaddr) { Console.WriteLine("FAIL (ACK yiaddr differs)"); return false; }

            // --- REQUEST outside pool (fresh MAC) -> NAK
            cli.SendTo(BuildDhcp(0xDEADBEEF, mac2, 3, requested: 0xC0A81FFA, serverId: 0xC0A81F01), srv);
            var nak = RecvDhcp(cli);
            if (nak is null) { Console.WriteLine("FAIL (no NAK)"); return false; }
            if (GetOpt(nak, 53) is not { } t3 || t3[0] != 6) { Console.WriteLine("FAIL (option 53 != NAK)"); return false; }

            // --- RELEASE
            cli.SendTo(BuildDhcp(0x12345678, mac1, 7, ciaddr: yiaddr), srv);
            Console.WriteLine("PASS");
            return true;
        }
        finally
        {
            server.Stop();
        }
    }

    private static byte[]? RecvDhcp(Socket cli)
    {
        var buf = new byte[2048];
        EndPoint from = new IPEndPoint(IPAddress.Any, 0);
        try
        {
            int n = cli.ReceiveFrom(buf, ref from);
            return n >= 240 ? buf[..n] : null;
        }
        catch (SocketException)
        {
            return null;
        }
    }

    private static byte[] BuildDhcp(uint xid, byte[] mac, byte msgType, uint? requested = null, uint? serverId = null, uint ciaddr = 0)
    {
        var b = new byte[264];
        b[0] = 1; b[1] = 1; b[2] = 6;
        PutBe32(b, 4, xid);
        PutBe32(b, 12, ciaddr);
        Array.Copy(mac, 0, b, 28, 6);
        PutBe32(b, 236, 0x63825363);
        int o = 240;
        b[o++] = 53; b[o++] = 1; b[o++] = msgType;
        if (requested is uint rq) { b[o++] = 50; b[o++] = 4; PutBe32(b, o, rq); o += 4; }
        if (serverId is uint sid) { b[o++] = 54; b[o++] = 4; PutBe32(b, o, sid); o += 4; }
        b[o++] = 255;
        return b;
    }

    private static byte[]? GetOpt(byte[] pkt, int code)
    {
        int i = 240;
        while (i < pkt.Length)
        {
            byte c = pkt[i];
            if (c == 255) break;
            if (c == 0) { i++; continue; }
            if (i + 1 >= pkt.Length) break;
            int len = pkt[i + 1];
            if (i + 2 + len > pkt.Length) break;
            if (c == code) return pkt[(i + 2)..(i + 2 + len)];
            i += 2 + len;
        }
        return null;
    }

    // ================================================================ TFTP

    private static bool TestTftpRrq()
    {
        Console.Write("TFTP RRQ (OACK + data integrity) ... ");
        string root = MakeTempRoot();
        try
        {
            var rng = new Random(42);
            var fileBytes = new byte[100_000];
            rng.NextBytes(fileBytes);
            File.WriteAllBytes(Path.Combine(root, "selftest_rom.bin"), fileBytes);
            var exactBytes = new byte[1024];
            rng.NextBytes(exactBytes);
            File.WriteAllBytes(Path.Combine(root, "exact.bin"), exactBytes);

            var server = new TftpServer(root, bindAddress: IPAddress.Loopback);
            server.Log += _ => { };
            server.Start();
            try
            {
                using var cli = new Socket(AddressFamily.InterNetwork, SocketType.Dgram, ProtocolType.Udp);
                cli.Bind(new IPEndPoint(IPAddress.Loopback, 0));
                cli.ReceiveTimeout = 3000;
                var srv = new IPEndPoint(IPAddress.Loopback, TftpServer.ServerPort);

                if (!DownloadFile(cli, srv, "selftest_rom.bin", fileBytes, out string? err)) { Console.WriteLine($"FAIL ({err})"); return false; }
                // exact multiple of 512 -> must include a zero-length final block (3 DATA packets)
                if (!DownloadFile(cli, srv, "exact.bin", exactBytes, out err, expectPackets: 3)) { Console.WriteLine($"FAIL ({err})"); return false; }
                Console.WriteLine("PASS");
                return true;
            }
            finally { server.Stop(); }
        }
        finally { CleanupRoot(root); }
    }

    private static bool DownloadFile(Socket cli, IPEndPoint srv, string fileName, byte[] expected, out string? err, int? expectPackets = null)
    {
        err = null;
        var rrq = new List<byte> { 0, 1 };
        rrq.AddRange(Encoding.ASCII.GetBytes(fileName)); rrq.Add(0);
        rrq.AddRange(Encoding.ASCII.GetBytes("octet")); rrq.Add(0);
        rrq.AddRange(Encoding.ASCII.GetBytes("blksize")); rrq.Add(0); rrq.AddRange(Encoding.ASCII.GetBytes("512")); rrq.Add(0);
        rrq.AddRange(Encoding.ASCII.GetBytes("timeout")); rrq.Add(0); rrq.AddRange(Encoding.ASCII.GetBytes("5")); rrq.Add(0);
        cli.SendTo(rrq.ToArray(), srv);

        var buf = new byte[2048];
        EndPoint from = new IPEndPoint(IPAddress.Any, 0);
        int n;
        try { n = cli.ReceiveFrom(buf, ref from); }
        catch (SocketException) { err = "timeout waiting OACK"; return false; }
        int op = (buf[0] << 8) | buf[1];
        if (op != 6) { err = "expected OACK"; return false; }

        cli.SendTo(new byte[] { 0, 4, 0, 0 }, from);

        var received = new MemoryStream();
        int block = 1, packets = 0;
        bool last = false;
        while (!last)
        {
            try { n = cli.ReceiveFrom(buf, ref from); }
            catch (SocketException) { err = "timeout waiting DATA"; return false; }
            op = (buf[0] << 8) | buf[1];
            if (op == 5) { err = "server ERROR"; return false; }
            if (op != 3) continue;
            int blk = (buf[2] << 8) | buf[3];
            if (blk != block) { err = $"block {blk} != {block}"; return false; }
            int payload = n - 4;
            received.Write(buf, 4, payload);
            cli.SendTo(new byte[] { 0, 4, (byte)(block >> 8), (byte)block }, from);
            packets++;
            if (payload < 512) last = true;
            block++;
        }
        if (expectPackets is int ep && packets != ep) { err = $"packets {packets} != {ep}"; return false; }
        if (!received.ToArray().SequenceEqual(expected)) { err = "data mismatch"; return false; }
        return true;
    }

    private static bool TestTftpWrq()
    {
        Console.Write("TFTP WRQ (upload) ... ");
        string root = MakeTempRoot();
        try
        {
            var rng = new Random(7);
            var payload = new byte[50_000];
            rng.NextBytes(payload);

            var server = new TftpServer(root, bindAddress: IPAddress.Loopback);
            server.Log += _ => { };
            server.Start();
            try
            {
                using var cli = new Socket(AddressFamily.InterNetwork, SocketType.Dgram, ProtocolType.Udp);
                cli.Bind(new IPEndPoint(IPAddress.Loopback, 0));
                cli.ReceiveTimeout = 3000;
                var srv = new IPEndPoint(IPAddress.Loopback, TftpServer.ServerPort);

                var wrq = new List<byte> { 0, 2 };
                wrq.AddRange(Encoding.ASCII.GetBytes("upload.bin")); wrq.Add(0);
                wrq.AddRange(Encoding.ASCII.GetBytes("octet")); wrq.Add(0);
                cli.SendTo(wrq.ToArray(), srv);

                var buf = new byte[2048];
                EndPoint from = new IPEndPoint(IPAddress.Any, 0);
                int n;
                try { n = cli.ReceiveFrom(buf, ref from); }
                catch (SocketException) { Console.WriteLine("FAIL (no ACK0)"); return false; }
                if ((buf[0] << 8 | buf[1]) != 4 || (buf[2] << 8 | buf[3]) != 0) { Console.WriteLine("FAIL (expected ACK 0)"); return false; }

                int block = 1, sent = 0;
                while (true)
                {
                    int len = Math.Min(512, payload.Length - sent);
                    var pkt = new byte[4 + len];
                    pkt[1] = 3; pkt[2] = (byte)(block >> 8); pkt[3] = (byte)block;
                    Array.Copy(payload, sent, pkt, 4, len);
                    cli.SendTo(pkt, from);
                    try { n = cli.ReceiveFrom(buf, ref from); }
                    catch (SocketException) { Console.WriteLine("FAIL (timeout waiting ACK)"); return false; }
                    if ((buf[0] << 8 | buf[1]) != 4 || (buf[2] << 8 | buf[3]) != block) { Console.WriteLine("FAIL (bad ACK)"); return false; }
                    sent += len;
                    if (len < 512) break;
                    block++;
                }

                var got = File.ReadAllBytes(Path.Combine(root, "upload.bin"));
                if (!got.SequenceEqual(payload)) { Console.WriteLine("FAIL (data mismatch)"); return false; }
                Console.WriteLine("PASS");
                return true;
            }
            finally { server.Stop(); }
        }
        finally { CleanupRoot(root); }
    }

    private static bool TestTftpNotFound()
    {
        Console.Write("TFTP RRQ missing file -> ERROR ... ");
        string root = MakeTempRoot();
        try
        {
            var server = new TftpServer(root, bindAddress: IPAddress.Loopback);
            server.Log += _ => { };
            server.Start();
            try
            {
                using var cli = new Socket(AddressFamily.InterNetwork, SocketType.Dgram, ProtocolType.Udp);
                cli.Bind(new IPEndPoint(IPAddress.Loopback, 0));
                cli.ReceiveTimeout = 3000;
                var rrq = new List<byte> { 0, 1 };
                rrq.AddRange(Encoding.ASCII.GetBytes("nope.bin")); rrq.Add(0);
                rrq.AddRange(Encoding.ASCII.GetBytes("octet")); rrq.Add(0);
                cli.SendTo(rrq.ToArray(), new IPEndPoint(IPAddress.Loopback, TftpServer.ServerPort));

                var buf = new byte[2048];
                EndPoint from = new IPEndPoint(IPAddress.Any, 0);
                int n;
                try { n = cli.ReceiveFrom(buf, ref from); }
                catch (SocketException) { Console.WriteLine("FAIL (timeout)"); return false; }
                if ((buf[0] << 8 | buf[1]) != 5) { Console.WriteLine("FAIL (expected ERROR)"); return false; }
                Console.WriteLine("PASS");
                return true;
            }
            finally { server.Stop(); }
        }
        finally { CleanupRoot(root); }
    }

    // ================================================================ ROM API parsing

    private static bool TestRomParse()
    {
        Console.Write("ROM list parsing (incl. double-scheme bug) ... ");
        const string sample =
            "小米路由器 4|http://bigota.miwifi.com/xiaoqiang/rom/r4/miwifi_r4_firmware_f1bbb_2.26.145.bin|16354096|" +
            "小米路由器 4Q|http://http://bigota.miwifi.com/xiaoqiang/rom/r4c/miwifi_r4c_all_ea31e_2.30.11.bin|9962408|" +
            "小米路由器 4C|http://bigota.miwifi.com/xiaoqiang/rom/r4cm/miwifi_r4cm_firmware_23bd4_2.14.67.bin|10224568\t";
        var list = RomService.Parse(sample);
        if (list.Count != 3) { Console.WriteLine($"FAIL (count {list.Count})"); return false; }
        if (!list[1].Url.StartsWith("http://bigota.miwifi.com", StringComparison.OrdinalIgnoreCase) ||
            list[1].Url.Contains("http://http://", StringComparison.OrdinalIgnoreCase))
        { Console.WriteLine("FAIL (double scheme not sanitized)"); return false; }
        if (list[0].Size != 16354096) { Console.WriteLine("FAIL (size)"); return false; }
        if (!list[0].Url.EndsWith("miwifi_r4_firmware_f1bbb_2.26.145.bin", StringComparison.OrdinalIgnoreCase))
        { Console.WriteLine("FAIL (filename)"); return false; }
        Console.WriteLine("PASS");
        return true;
    }

    /// <summary>Live check against Xiaomi's cloud (SKIPs when offline; fails only if reachable but broken).</summary>
    private static void TestRomLive()
    {
        Console.Write("Live ROM list from api.miwifi.com ... ");
        try
        {
            var list = RomService.GetRomListAsync().GetAwaiter().GetResult();
            if (list.Count == 0) { Console.WriteLine("FAIL (empty list from API)"); return; }
            Console.WriteLine($"PASS ({list.Count} models, e.g. {list[0].Name} -> {Path.GetFileName(new Uri(list[0].Url).AbsolutePath)})");
        }
        catch (Exception ex)
        {
            Console.WriteLine($"SKIP (offline? {ex.Message})");
        }
    }

    // ================================================================ helpers

    private static string MakeTempRoot()
    {
        string root = Path.Combine(Path.GetTempPath(), "miwifi_selftest_" + Guid.NewGuid().ToString("N"));
        Directory.CreateDirectory(root);
        return root;
    }

    private static void CleanupRoot(string root)
    {
        try { Directory.Delete(root, true); } catch { /* ignore */ }
    }

    private static uint Be32(byte[] b, int o) =>
        (uint)((b[o] << 24) | (b[o + 1] << 16) | (b[o + 2] << 8) | b[o + 3]);

    private static void PutBe32(byte[] b, int o, uint v)
    {
        b[o] = (byte)(v >> 24); b[o + 1] = (byte)(v >> 16); b[o + 2] = (byte)(v >> 8); b[o + 3] = (byte)v;
    }
}
