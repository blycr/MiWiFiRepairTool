namespace MiWiFiRepairTool.Core;

/// <summary>
/// Thread-safe logger. Line format mirrors the original tool:
/// <c>[dd/MM HH:mm:ss.fff] message</c>.
/// </summary>
public sealed class Logger
{
    private readonly object _sync = new();

    /// <summary>Raised for every log line (with the timestamp prefix already applied).</summary>
    public event Action<string>? Line;

    /// <summary>Writes a line (thread-safe).</summary>
    public void Write(string message)
    {
        string line = $"[{DateTime.Now:dd/MM HH:mm:ss.fff}] {message}";
        lock (_sync)
        {
            Console.WriteLine(line);
        }
        Line?.Invoke(line);
    }

    /// <summary>Writes an empty line.</summary>
    public void Blank() => Write("");
}
