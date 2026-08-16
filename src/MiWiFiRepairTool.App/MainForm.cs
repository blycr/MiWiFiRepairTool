using System.ComponentModel;
using System.Diagnostics;
using System.Net;
using System.Windows.Forms;
using MiWiFiRepairTool.Core;

namespace MiWiFiRepairTool.App;

/// <summary>
/// Main window. Steps mirror the original tool: pick the NIC connected to the
/// router LAN port, pick the firmware (cloud download or local file), start the
/// DHCP + TFTP servers, then watch the transfer and the router's LED.
/// </summary>
public sealed class MainForm : Form
{
    private readonly Logger _log = new();
    private readonly string _appDir = AppContext.BaseDirectory;
    private readonly string[] _args;

    private ComboBox _nicCombo = null!;
    private Button _refreshNic = null!;
    private ComboBox _romCombo = null!;
    private Button _refreshRom = null!;
    private Button _download = null!;
    private Button _local = null!;
    private TextBox _fwPathBox = null!;
    private Button _start = null!;
    private Button _stop = null!;
    private Button _help = null!;
    private ProgressBar _progress = null!;
    private TextBox _logBox = null!;
    private ToolStripStatusLabel _statusLabel = null!;

    private List<RomInfo> _roms = new();
    private string? _fwPath;
    private NicInfo? _nicSnapshot;   // captured before flashing, used for restore
    private DhcpServer? _dhcp;
    private TftpServer? _tftp;

    public MainForm(string[] args)
    {
        _args = args;
        Text = "小米路由器修复工具（现代化重构版）";
        StartPosition = FormStartPosition.CenterScreen;
        ClientSize = new Size(860, 640);
        MinimumSize = new Size(820, 580);
        _log.Line += OnLogLine;

        BuildUi();
        LoadNicList();
        _ = LoadRomListAsync();
    }

    // ------------------------------------------------------------------ UI

    private void BuildUi()
    {
        var root = new TableLayoutPanel
        {
            Dock = DockStyle.Fill,
            Padding = new Padding(12),
            ColumnCount = 1,
            RowCount = 6,
        };
        root.RowStyles.Add(new RowStyle(SizeType.AutoSize));
        root.RowStyles.Add(new RowStyle(SizeType.AutoSize));
        root.RowStyles.Add(new RowStyle(SizeType.AutoSize));
        root.RowStyles.Add(new RowStyle(SizeType.AutoSize));
        root.RowStyles.Add(new RowStyle(SizeType.Percent, 100F));
        root.RowStyles.Add(new RowStyle(SizeType.AutoSize));
        Controls.Add(root);

        // ---- step 1: NIC -------------------------------------------------
        var nicGroup = new GroupBox { Text = "第 1 步：选择网卡（网线连接电脑和路由器 LAN 口）", Dock = DockStyle.Fill, Padding = new Padding(10, 6, 10, 6) };
        var nicLayout = new TableLayoutPanel { Dock = DockStyle.Fill, ColumnCount = 2, RowCount = 2 };
        nicLayout.ColumnStyles.Add(new ColumnStyle(SizeType.Percent, 100F));
        nicLayout.ColumnStyles.Add(new ColumnStyle(SizeType.AutoSize));
        _nicCombo = new ComboBox { DropDownStyle = ComboBoxStyle.DropDownList, Dock = DockStyle.Fill, Anchor = AnchorStyles.Left | AnchorStyles.Right };
        _refreshNic = new Button { Text = "刷新网卡", Width = 90, Anchor = AnchorStyles.Right };
        _refreshNic.Click += (_, _) => LoadNicList();
        nicLayout.Controls.Add(_nicCombo, 0, 0);
        nicLayout.Controls.Add(_refreshNic, 1, 0);
        nicLayout.Controls.Add(new Label
        {
            Text = "提示：多网卡环境请禁用其他网卡（包括虚拟网卡），防止网络干扰；开始刷机时工具会自动把该网卡设为 192.168.31.1，退出时自动恢复。",
            Dock = DockStyle.Fill,
            ForeColor = Color.Gray,
            AutoEllipsis = true,
        }, 0, 1);
        nicLayout.SetColumnSpan(nicLayout.GetControlFromPosition(0, 1)!, 2);
        nicGroup.Controls.Add(nicLayout);
        root.Controls.Add(nicGroup, 0, 0);

        // ---- step 2: firmware --------------------------------------------
        var fwGroup = new GroupBox { Text = "第 2 步：选择刷机包", Dock = DockStyle.Fill, Padding = new Padding(10, 6, 10, 6) };
        var fwLayout = new TableLayoutPanel { Dock = DockStyle.Fill, ColumnCount = 4, RowCount = 2 };
        fwLayout.ColumnStyles.Add(new ColumnStyle(SizeType.Percent, 100F));
        fwLayout.ColumnStyles.Add(new ColumnStyle(SizeType.AutoSize));
        fwLayout.ColumnStyles.Add(new ColumnStyle(SizeType.AutoSize));
        fwLayout.ColumnStyles.Add(new ColumnStyle(SizeType.AutoSize));
        _romCombo = new ComboBox { DropDownStyle = ComboBoxStyle.DropDownList, Dock = DockStyle.Fill, Anchor = AnchorStyles.Left | AnchorStyles.Right };
        _refreshRom = new Button { Text = "刷新列表", Width = 90, Anchor = AnchorStyles.Right };
        _refreshRom.Click += async (_, _) => await LoadRomListAsync();
        _download = new Button { Text = "下载刷机包", Width = 100, Anchor = AnchorStyles.Right };
        _download.Click += async (_, _) => await DownloadRomAsync();
        _fwPathBox = new TextBox { ReadOnly = true, Dock = DockStyle.Fill, Anchor = AnchorStyles.Left | AnchorStyles.Right };
        _fwPathBox.Text = "（未选择：请从云端下载，或点「本地上传」选择 .bin 文件）";
        _local = new Button { Text = "本地上传", Width = 100, Anchor = AnchorStyles.Right };
        _local.Click += (_, _) => BrowseLocal();
        fwLayout.Controls.Add(_romCombo, 0, 0);
        fwLayout.Controls.Add(_refreshRom, 1, 0);
        fwLayout.Controls.Add(_download, 2, 0);
        fwLayout.Controls.Add(_fwPathBox, 0, 1);
        fwLayout.Controls.Add(_local, 3, 1);
        fwGroup.Controls.Add(fwLayout);
        root.Controls.Add(fwGroup, 0, 1);

        // ---- step 3: action bar ------------------------------------------
        var actionPanel = new FlowLayoutPanel { Dock = DockStyle.Fill, WrapContents = false, Padding = new Padding(0, 8, 0, 8) };
        _start = new Button { Text = "开始刷机", Width = 120, Height = 34, BackColor = Color.FromArgb(225, 60, 60), ForeColor = Color.White, FlatStyle = FlatStyle.Flat };
        _start.Click += (_, _) => StartFlashing();
        _stop = new Button { Text = "停止", Width = 90, Height = 34, Enabled = false };
        _stop.Click += (_, _) => StopFlashing(restore: true);
        _help = new Button { Text = "使用说明", Width = 90, Height = 34 };
        _help.Click += (_, _) => ShowHelp();
        _progress = new ProgressBar { Width = 420, Height = 20, Minimum = 0, Maximum = 1000, Margin = new Padding(14, 8, 4, 0) };
        actionPanel.Controls.Add(_start);
        actionPanel.Controls.Add(_stop);
        actionPanel.Controls.Add(_help);
        actionPanel.Controls.Add(_progress);
        root.Controls.Add(actionPanel, 0, 2);

        // ---- log ----------------------------------------------------------
        _logBox = new TextBox
        {
            Multiline = true,
            ReadOnly = true,
            ScrollBars = ScrollBars.Vertical,
            Dock = DockStyle.Fill,
            Font = new Font("Consolas", 9F),
            BackColor = Color.FromArgb(16, 16, 18),
            ForeColor = Color.FromArgb(220, 220, 220),
        };
        root.Controls.Add(_logBox, 0, 4);

        // ---- status strip -------------------------------------------------
        var strip = new StatusStrip();
        _statusLabel = new ToolStripStatusLabel { Spring = true, TextAlign = ContentAlignment.MiddleLeft, Text = "就绪" };
        strip.Items.Add(_statusLabel);
        root.Controls.Add(strip, 0, 5);
    }

    // ------------------------------------------------------------------ NIC / ROM list

    private void LoadNicList()
    {
        string? prev = (_nicCombo.SelectedItem as NicInfo)?.Name;
        var nics = NicManager.Enumerate();
        _nicCombo.Items.Clear();
        foreach (var n in nics) _nicCombo.Items.Add(n);
        if (nics.Count == 0)
        {
            _log.Write("未发现可用的以太网/无线网卡。");
            return;
        }
        var pick = nics.FirstOrDefault(n => n.Name == prev)
                ?? nics.FirstOrDefault(n => n.Up && n.Ipv4 != null)
                ?? nics.FirstOrDefault(n => n.Up)
                ?? nics[0];
        _nicCombo.SelectedItem = pick;
    }

    private async Task LoadRomListAsync()
    {
        _refreshRom.Enabled = false;
        try
        {
            var roms = await RomService.GetRomListAsync();
            _roms = roms;
            _romCombo.Items.Clear();
            foreach (var r in roms)
                _romCombo.Items.Add($"{r.Name}（{r.Size / 1024.0 / 1024.0:F1} MB）");
            if (roms.Count > 0)
            {
                _romCombo.SelectedIndex = 0;
                _log.Write($"已从云端获取 {roms.Count} 个可用刷机包。");
            }
            else
            {
                _log.Write("云端未返回刷机包列表（可能已离线），请使用「本地上传」选择本地刷机包。");
            }
        }
        catch (Exception ex)
        {
            _log.Write($"获取云端刷机包列表失败：{ex.Message}。可使用「本地上传」选择本地刷机包。");
        }
        finally
        {
            _refreshRom.Enabled = true;
        }
    }

    // ------------------------------------------------------------------ actions

    private async Task DownloadRomAsync()
    {
        if (_romCombo.SelectedIndex < 0 || _romCombo.SelectedIndex >= _roms.Count)
        {
            MessageBox.Show(this, "请先在列表中选择路由器型号。", "提示", MessageBoxButtons.OK, MessageBoxIcon.Information);
            return;
        }
        var rom = _roms[_romCombo.SelectedIndex];
        _download.Enabled = false;
        _progress.Value = 0;
        try
        {
            _log.Write($"正在从云端下载 {rom.Name} 刷机包（{rom.Size / 1024.0 / 1024.0:F1} MB）...");
            var progress = new Progress<double>(p => _progress.Value = (int)(p * 1000));
            string dest = await RomService.DownloadAsync(rom, _appDir, progress);
            _fwPath = dest;
            _fwPathBox.Text = dest;
            _statusLabel.Text = $"刷机包就绪：{Path.GetFileName(dest)}";
            _progress.Value = 1000;
            _log.Write($"刷机包下载完成：{dest}");
        }
        catch (Exception ex)
        {
            _progress.Value = 0;
            _statusLabel.Text = "下载失败";
            _log.Write($"下载失败：{ex.Message}");
        }
        finally
        {
            _download.Enabled = true;
        }
    }

    private void BrowseLocal()
    {
        using var ofd = new OpenFileDialog
        {
            Title = "选择刷机包（.bin）",
            Filter = "刷机包 (*.bin)|*.bin|所有文件 (*.*)|*.*",
        };
        if (ofd.ShowDialog(this) != DialogResult.OK) return;
        _fwPath = ofd.FileName;
        _fwPathBox.Text = _fwPath;
        _statusLabel.Text = $"刷机包就绪：{Path.GetFileName(_fwPath)}";
        _log.Write($"已选择刷机包：{_fwPath}（开始刷机时如不在程序目录，会自动复制过去）");
    }

    private void StartFlashing()
    {
        if (_dhcp is not null || _tftp is not null) return;

        var nic = _nicCombo.SelectedItem as NicInfo;
        if (nic is null)
        {
            MessageBox.Show(this, "请先选择网卡。", "提示", MessageBoxButtons.OK, MessageBoxIcon.Information);
            return;
        }
        if (string.IsNullOrEmpty(_fwPath) || !File.Exists(_fwPath))
        {
            MessageBox.Show(this, "请先选择刷机包（云端下载或本地上传）。", "提示", MessageBoxButtons.OK, MessageBoxIcon.Information);
            return;
        }

        if (!NicManager.IsAdministrator())
        {
            // request elevation only when actually needed (configuring the NIC)
            var psi = new ProcessStartInfo
            {
                FileName = Environment.ProcessPath!,
                Arguments = $"--elevated --nic \"{nic.Name}\" --fw \"{_fwPath}\"",
                Verb = "runas",
                UseShellExecute = true,
            };
            try
            {
                _log.Write("正在请求管理员权限以配置网卡并启动刷机服务...");
                Process.Start(psi);
                Close();
            }
            catch (Win32Exception)
            {
                MessageBox.Show(this, "需要管理员权限才能配置网卡并启动刷机服务。", "提示", MessageBoxButtons.OK, MessageBoxIcon.Warning);
            }
            return;
        }

        // the TFTP root is the program directory; make sure the firmware lives there
        string tftpRoot = _appDir;
        string fw = _fwPath;
        string rootFull = Path.GetFullPath(tftpRoot);
        string fwFull = Path.GetFullPath(fw);
        if (!fwFull.StartsWith(rootFull, StringComparison.OrdinalIgnoreCase))
        {
            try
            {
                fw = Path.Combine(tftpRoot, Path.GetFileName(fw));
                File.Copy(_fwPath, fw, overwrite: true);
                _log.Write($"已将刷机包复制到程序目录：{fw}");
            }
            catch (Exception ex)
            {
                MessageBox.Show(this, $"无法将刷机包复制到程序目录：{ex.Message}", "错误", MessageBoxButtons.OK, MessageBoxIcon.Error);
                return;
            }
        }
        _fwPath = fw;

        try
        {
            _log.Write($"正在配置网卡「{nic.Name}」为 {NicManager.StaticIp}/{NicManager.StaticMask} ...");
            NicManager.SetStatic(nic.Name);
            _nicSnapshot = nic;
            _log.Write("网卡配置已更改，退出将恢复为自动分配状态");
        }
        catch (Exception ex)
        {
            MessageBox.Show(this, $"网卡配置失败：{ex.Message}", "错误", MessageBoxButtons.OK, MessageBoxIcon.Error);
            _nicSnapshot = null;
            return;
        }

        try
        {
            _dhcp = new DhcpServer(IPAddress.Parse(NicManager.StaticIp), IPAddress.Parse("192.168.31.100"), 100);
            _dhcp.Log += s => _log.Write(s);
            _dhcp.Start();

            _tftp = new TftpServer(tftpRoot);
            _tftp.Log += s => _log.Write(s);
            _tftp.TransferUpdated += OnTransfer;
            _tftp.Start();

            AddFirewallRule();
            SetRunningState(true);
            _log.Blank();
            _log.Write("刷机服务已启动（DHCP 地址池 192.168.31.100 - 192.168.31.199，TFTP 根目录为程序目录）。");
            _log.Write("请确认：路由器已通电，网线已连接电脑和路由器 LAN 口（刷机时请不要插外网网线）。");
            _log.Write("刷机方法：拔掉路由器电源 → 按住 Reset 键不松手 → 重新上电 → 等待指示灯进入刷机流程后松开 Reset（Mesh 机型等紫灯常亮再松开）。");
            _log.Write("请稍等几分钟，路由器蓝灯闪烁表示刷机成功，然后请断电重启路由器！");
        }
        catch (Exception ex)
        {
            StopFlashing(restore: true);
            MessageBox.Show(this, $"无法启动刷机服务：{ex.Message}\n（端口 67/69 可能被其他程序占用）", "错误", MessageBoxButtons.OK, MessageBoxIcon.Error);
        }
    }

    private void StopFlashing(bool restore)
    {
        if (_tftp is not null) { try { _tftp.Stop(); } catch { /* ignore */ } }
        if (_dhcp is not null) { try { _dhcp.Stop(); } catch { /* ignore */ } }
        _tftp = null;
        _dhcp = null;
        RemoveFirewallRule();
        if (restore && _nicSnapshot is not null)
        {
            try
            {
                NicManager.Restore(_nicSnapshot);
                _log.Write("网卡配置已恢复。");
            }
            catch (Exception ex)
            {
                _log.Write($"恢复网卡配置失败：{ex.Message}");
            }
            _nicSnapshot = null;
        }
        SetRunningState(false);
    }

    private void SetRunningState(bool running)
    {
        _start.Enabled = !running;
        _stop.Enabled = running;
        _nicCombo.Enabled = !running;
        _refreshNic.Enabled = !running;
        _romCombo.Enabled = !running;
        _refreshRom.Enabled = !running;
        _download.Enabled = !running;
        _local.Enabled = !running;
        _statusLabel.Text = running ? "刷机服务运行中" : "就绪";
    }

    // ------------------------------------------------------------------ transfer progress

    private void OnTransfer(TftpServer.TransferInfo info)
    {
        if (IsDisposed || Disposing) return;
        try
        {
            BeginInvoke(() =>
            {
                if (info.Done)
                {
                    if (info.Error is not null)
                    {
                        _statusLabel.Text = $"传输失败：{info.Error}";
                        _progress.Value = 0;
                    }
                    else
                    {
                        _statusLabel.Text = $"{info.FileName} 传输完成：{info.BytesSent:N0} 字节，{info.Blocks:N0} 块，{info.Seconds:F0} 秒，重传 {info.Retransmits} 次";
                        _progress.Value = 1000;
                    }
                    return;
                }
                if (info.TotalBytes > 0)
                {
                    _progress.Value = (int)Math.Min(1000, info.BytesSent * 1000.0 / Math.Max(1, info.TotalBytes));
                    double kbs = info.Seconds > 0 ? info.BytesSent / 1024.0 / info.Seconds : 0;
                    _statusLabel.Text = $"正在传输 {info.FileName}：{kbs:F0} KB/s，{info.Blocks:N0} 块，重传 {info.Retransmits} 次";
                }
                else
                {
                    _statusLabel.Text = $"正在接收 {info.FileName}：{info.BytesSent:N0} 字节";
                }
            });
        }
        catch (InvalidOperationException)
        {
            // window handle not created yet
        }
    }

    // ------------------------------------------------------------------ log

    private void OnLogLine(string line)
    {
        if (IsDisposed || Disposing) return;
        try
        {
            BeginInvoke(() =>
            {
                const int maxChars = 60_000;
                if (_logBox.TextLength > maxChars) _logBox.Text = _logBox.Text[^maxChars..];
                _logBox.AppendText(line + Environment.NewLine);
            });
        }
        catch (InvalidOperationException)
        {
            // window not ready
        }
    }

    // ------------------------------------------------------------------ help / misc

    private void ShowHelp()
    {
        MessageBox.Show(this, HelpText, "小米路由器修复工具使用说明", MessageBoxButtons.OK, MessageBoxIcon.Information);
    }

    protected override void OnFormClosing(FormClosingEventArgs e)
    {
        if (_dhcp is not null || _tftp is not null)
        {
            var r = MessageBox.Show(this,
                "刷机服务仍在运行。退出将停止服务并恢复网卡配置，确定退出吗？",
                "提示", MessageBoxButtons.OKCancel, MessageBoxIcon.Warning);
            if (r != DialogResult.OK)
            {
                e.Cancel = true;
                return;
            }
            StopFlashing(restore: true);
        }
        base.OnFormClosing(e);
    }

    protected override void OnShown(EventArgs e)
    {
        base.OnShown(e);
        // elevated relaunch: auto-start flashing with the passed arguments
        if (_args.Contains("--elevated", StringComparer.OrdinalIgnoreCase))
        {
            string? nicName = Arg("--nic");
            string? fwPath = Arg("--fw");
            if (nicName is not null)
            {
                var nic = _nicCombo.Items.Cast<NicInfo>().FirstOrDefault(n => n.Name == nicName);
                if (nic is not null) _nicCombo.SelectedItem = nic;
            }
            if (fwPath is not null && File.Exists(fwPath))
            {
                _fwPath = fwPath;
                _fwPathBox.Text = fwPath;
            }
            StartFlashing();
        }
    }

    private string? Arg(string name)
    {
        for (int i = 0; i < _args.Length - 1; i++)
            if (_args[i].Equals(name, StringComparison.OrdinalIgnoreCase)) return _args[i + 1];
        return null;
    }

    private static void AddFirewallRule() => RunFirewall("add rule name=\"MiWiFiRepairTool\" dir=in action=allow protocol=UDP localport=67,69 profile=private");
    private static void RemoveFirewallRule() => RunFirewall("delete rule name=\"MiWiFiRepairTool\"");

    private static void RunFirewall(string args)
    {
        try
        {
            var psi = new ProcessStartInfo("netsh", $"advfirewall firewall {args}")
            {
                UseShellExecute = false,
                CreateNoWindow = true,
                RedirectStandardOutput = true,
                RedirectStandardError = true,
            };
            using var p = Process.Start(psi);
            p?.WaitForExit(15_000);
        }
        catch
        {
            // firewall rules are best-effort
        }
    }

    private const string HelpText =
        "一、刷机前准备\n" +
        "    将路由器通电，并用网线连接电脑和路由器 LAN 口。\n\n" +
        "二、操作说明\n" +
        "  1. 网卡设置：将使用管理员权限为用户更改网卡配置，以确保路由器和电脑处于同一局域网。关闭时会提醒并自动恢复网卡配置。\n" +
        "  2. 刷机成功后，需断电重启路由器。\n\n" +
        "三、注意事项\n" +
        "  1. 刷机需要花费几分钟时间，过程中请不要关闭电源和应用程序。\n" +
        "  2. 本工具仅支持小米路由器 4 及以上系列路由器刷机使用。\n" +
        "  3. 刷机适用于路由器红灯问题，其他问题可咨询客服或售后。\n" +
        "  4. 多次刷机失败，请联系售后服务热线：400-100-5678（周一至周日 8:00-18:00）。\n\n" +
        "四、路由器指示灯状态说明\n" +
        "  1. 蓝灯长亮：工作正常。\n" +
        "  2. 蓝灯闪烁：刷机成功（需要断电重启）。\n" +
        "  3. 橙灯长亮：正在启动。\n" +
        "  4. 橙灯闪烁：进入刷机流程或系统升级中。\n" +
        "  5. 红灯长亮：系统故障。\n" +
        "  6. 红灯闪烁：刷机失败。\n\n" +
        "  如果您配置的是小米路由器 Mesh，指示灯状态如下\n" +
        "  7. 紫灯常亮：进入刷机流程，或 Mesh 已完成配置但与上级路由器连接质量较差。\n" +
        "  8. 紫灯闪烁：Mesh 已完成配置但与上级路由器失去连接。\n" +
        "  9. 绿灯闪烁：手机已连接到路由器，正在配置上网。";
}
