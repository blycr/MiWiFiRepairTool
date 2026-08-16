using System.Net.Http;

namespace MiWiFiRepairTool.Core;

/// <summary>A ROM entry as returned by Xiaomi's repair-tool API.</summary>
public sealed record RomInfo(string Name, string Url, long Size);

/// <summary>
/// Talks to Xiaomi's cloud service that the original tool used:
/// GET http://api.miwifi.com/data/tffp_rom_link_info returns a pipe-separated
/// list of <c>显示名|下载URL|大小(字节)</c> triples. The URL basename is the
/// exact TFTP filename the router requests during recovery flashing, so it is
/// used as the local file name.
/// </summary>
public static class RomService
{
    public const string DefaultApiUrl = "http://api.miwifi.com/data/tffp_rom_link_info";

    private static readonly HttpClient Http = new()
    {
        Timeout = TimeSpan.FromSeconds(90),
    };

    static RomService()
    {
        Http.DefaultRequestHeaders.UserAgent.ParseAdd("Mozilla/5.0 MiWiFiRepairTool/2.0");
    }

    /// <summary>Fetches and parses the current model -> ROM list.</summary>
    public static async Task<List<RomInfo>> GetRomListAsync(string? apiUrl = null, CancellationToken ct = default)
    {
        string url = string.IsNullOrWhiteSpace(apiUrl) ? DefaultApiUrl : apiUrl;
        string body = await Http.GetStringAsync(url, ct).ConfigureAwait(false);
        return Parse(body);
    }

    public static List<RomInfo> Parse(string body)
    {
        var parts = body.Trim().Split('|');
        var list = new List<RomInfo>();
        for (int i = 0; i + 2 < parts.Length; i += 3)
        {
            string name = parts[i].Trim();
            string url = SanitizeUrl(parts[i + 1].Trim());
            long size = long.TryParse(parts[i + 2].Trim(), out long s) ? s : 0;
            if (name.Length == 0 || url.Length == 0) continue;
            list.Add(new RomInfo(name, url, size));
        }
        return list;
    }

    /// <summary>Fixes the known server-side bug where a URL is prefixed twice.</summary>
    public static string SanitizeUrl(string url)
    {
        while (url.StartsWith("http://http://", StringComparison.OrdinalIgnoreCase) ||
               url.StartsWith("https://http://", StringComparison.OrdinalIgnoreCase))
        {
            int cut = url.IndexOf("://", StringComparison.Ordinal) + 3;
            url = url[cut..];
        }
        return url;
    }

    /// <summary>
    /// Downloads the ROM into <paramref name="destDir"/> under its URL basename
    /// (the TFTP filename). Verifies the size reported by the API. Returns the
    /// full destination path.
    /// </summary>
    public static async Task<string> DownloadAsync(RomInfo rom, string destDir, IProgress<double>? progress = null, CancellationToken ct = default)
    {
        Directory.CreateDirectory(destDir);
        string fileName = Path.GetFileName(new Uri(rom.Url).AbsolutePath);
        if (string.IsNullOrEmpty(fileName)) fileName = "firmware.bin";
        string dest = Path.Combine(destDir, fileName);

        using var resp = await Http.GetAsync(rom.Url, HttpCompletionOption.ResponseHeadersRead, ct).ConfigureAwait(false);
        resp.EnsureSuccessStatusCode();
        await using var src = await resp.Content.ReadAsStreamAsync(ct).ConfigureAwait(false);
        await using var dst = File.Create(dest);

        var buffer = new byte[64 * 1024];
        long total = 0;
        int n;
        while ((n = await src.ReadAsync(buffer, ct).ConfigureAwait(false)) > 0)
        {
            await dst.WriteAsync(buffer.AsMemory(0, n), ct).ConfigureAwait(false);
            total += n;
            progress?.Report(rom.Size > 0 ? (double)total / rom.Size : 0);
        }

        if (rom.Size > 0 && total != rom.Size)
        {
            try { File.Delete(dest); } catch { /* ignore */ }
            throw new InvalidDataException($"刷机包文件大小异常：期望 {rom.Size} 字节，实际 {total} 字节，请检查网络后重试");
        }
        return dest;
    }
}
