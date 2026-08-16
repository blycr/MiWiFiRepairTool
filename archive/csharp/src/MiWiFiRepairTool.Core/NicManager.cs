using System.ComponentModel;
using System.Diagnostics;
using System.Net;
using System.Net.NetworkInformation;
using System.Net.Sockets;
using System.Runtime.InteropServices;
using System.Text;

namespace MiWiFiRepairTool.Core;

/// <summary>Snapshot of a network adapter that the tool can configure.</summary>
public sealed class NicInfo
{
    public required string Name { get; init; }          // friendly name used by netsh
    public required string Description { get; init; }
    public required string Mac { get; init; }
    public required bool Up { get; init; }
    public required bool IsDhcp { get; init; }
    public required string? Ipv4 { get; init; }
    public required string? Ipv4Mask { get; init; }
    public required string? Gateway { get; init; }
    public required IReadOnlyList<string> Dns { get; init; }

    public override string ToString() =>
        $"{Name}  [{Ipv4 ?? "无IP"}]  {Mac}{(Up ? "" : "  (已断开)")}";
}

/// <summary>
/// Enumerates adapters and configures the selected one via <c>netsh</c>:
/// set a static 192.168.31.1/24 address before flashing, and restore the
/// previous configuration (DHCP or static) afterwards. The DHCP-enabled flag
/// comes from iphlpapi's GetAdaptersInfo so it is locale-independent.
/// </summary>
public static class NicManager
{
    public const string StaticIp = "192.168.31.1";
    public const string StaticMask = "255.255.255.0";

    // ------------------------------------------------------------------ enumeration

    public static List<NicInfo> Enumerate()
    {
        var dhcpMap = GetDhcpEnabledMap();
        var result = new List<NicInfo>();
        foreach (var ni in NetworkInterface.GetAllNetworkInterfaces())
        {
            if (ni.NetworkInterfaceType is NetworkInterfaceType.Loopback or NetworkInterfaceType.Tunnel) continue;
            if (ni.NetworkInterfaceType is not (NetworkInterfaceType.Ethernet or NetworkInterfaceType.Wireless80211)) continue;

            var props = ni.GetIPProperties();
            var ipv4 = props.UnicastAddresses
                .FirstOrDefault(a => a.Address.AddressFamily == AddressFamily.InterNetwork && !IPAddress.IsLoopback(a.Address));
            string mac = string.Join(":", ni.GetPhysicalAddress().GetAddressBytes().Select(b => b.ToString("X2")));
            if (mac.Length == 0) mac = "无";

            result.Add(new NicInfo
            {
                Name = ni.Name,
                Description = ni.Description?.Trim() ?? "",
                Mac = mac,
                Up = ni.OperationalStatus == OperationalStatus.Up,
                IsDhcp = dhcpMap.TryGetValue(ni.Description?.Trim() ?? "", out bool dhcp) ? dhcp : true,
                Ipv4 = ipv4?.Address.ToString(),
                Ipv4Mask = ipv4?.IPv4Mask?.ToString(),
                Gateway = props.GatewayAddresses.FirstOrDefault()?.Address.ToString(),
                Dns = props.DnsAddresses.Select(d => d.ToString()).ToList(),
            });
        }
        return result;
    }

    // ------------------------------------------------------------------ configuration

    /// <summary>Sets the adapter to the static 192.168.31.1/24 address (requires admin).</summary>
    public static void SetStatic(string nicName)
    {
        RunNetsh($"interface ip set address name=\"{nicName}\" static {StaticIp} {StaticMask}");
    }

    /// <summary>Restores the adapter to the configuration captured before flashing.</summary>
    public static void Restore(NicInfo nic)
    {
        if (nic.IsDhcp)
        {
            RunNetsh($"interface ip set address name=\"{nic.Name}\" dhcp");
            RunNetsh($"interface ip set dns name=\"{nic.Name}\" dhcp");
            return;
        }

        var args = $"interface ip set address name=\"{nic.Name}\" static {nic.Ipv4} {nic.Ipv4Mask ?? StaticMask}";
        if (!string.IsNullOrEmpty(nic.Gateway)) args += $" {nic.Gateway} 1";
        RunNetsh(args);

        if (nic.Dns.Count > 0)
        {
            RunNetsh($"interface ip set dns name=\"{nic.Name}\" static {nic.Dns[0]}");
            for (int i = 1; i < nic.Dns.Count; i++)
                RunNetsh($"interface ip add dns name=\"{nic.Name}\" {nic.Dns[i]}");
        }
    }

    public static bool IsAdministrator()
    {
        using var identity = System.Security.Principal.WindowsIdentity.GetCurrent();
        var principal = new System.Security.Principal.WindowsPrincipal(identity);
        return principal.IsInRole(System.Security.Principal.WindowsBuiltInRole.Administrator);
    }

    private static void RunNetsh(string args)
    {
        var psi = new ProcessStartInfo("netsh", args)
        {
            UseShellExecute = false,
            CreateNoWindow = true,
            RedirectStandardOutput = true,
            RedirectStandardError = true,
        };
        using var p = Process.Start(psi)
            ?? throw new InvalidOperationException("无法启动 netsh");
        string output = p.StandardOutput.ReadToEnd() + p.StandardError.ReadToEnd();
        if (!p.WaitForExit(30_000))
        {
            try { p.Kill(); } catch { /* ignore */ }
            throw new TimeoutException("netsh 执行超时");
        }
        if (p.ExitCode != 0)
            throw new Win32Exception(p.ExitCode, $"netsh {args} 失败：{output.Trim()}");
    }

    // ------------------------------------------------------------------ DHCP flag via GetAdaptersInfo

    [DllImport("iphlpapi.dll", CharSet = CharSet.Ansi)]
    private static extern uint GetAdaptersInfo(IntPtr pAdapterInfo, ref uint pOutBufLen);

    private static Dictionary<string, bool> GetDhcpEnabledMap()
    {
        var map = new Dictionary<string, bool>(StringComparer.OrdinalIgnoreCase);
        uint len = 0;
        if (GetAdaptersInfo(IntPtr.Zero, ref len) != 0x6F /* ERROR_BUFFER_OVERFLOW */ || len == 0) return map;

        IntPtr buf = Marshal.AllocHGlobal((int)len);
        try
        {
            if (GetAdaptersInfo(buf, ref len) != 0) return map;

            int ptr = IntPtr.Size;
            int offName = ptr + 4;
            int offDesc = offName + 260;
            int offAddrLen = offDesc + 132;
            int offAddr = offAddrLen + 4;
            int offIndex = offAddr + 8;
            int offType = offIndex + 4;
            int offDhcp = offType + 4;
            int offCurIp = Align(offDhcp + 4, ptr);
            int offIpList = offCurIp + ptr;

            IntPtr p = buf;
            while (p != IntPtr.Zero)
            {
                string desc = Marshal.PtrToStringAnsi(p + offDesc) ?? "";
                int dhcp = Marshal.ReadInt32(p, offDhcp);
                map[desc.Trim()] = dhcp != 0;
                p = Marshal.ReadIntPtr(p);
            }
        }
        finally
        {
            Marshal.FreeHGlobal(buf);
        }
        return map;
    }

    private static int Align(int value, int alignment) => (value + alignment - 1) & ~(alignment - 1);
}
