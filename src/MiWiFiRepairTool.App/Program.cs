using System.Windows.Forms;
using MiWiFiRepairTool.App;

namespace MiWiFiRepairTool.App;

internal static class Program
{
    [STAThread]
    private static int Main(string[] args)
    {
        if (args.Length > 0 && args[0].Equals("--selftest", StringComparison.OrdinalIgnoreCase))
            return SelfTest.Run();

        ApplicationConfiguration.Initialize();
        Application.Run(new MainForm(args));
        return 0;
    }
}
