# Saves a PNG of a running app's main window, even when it is behind others.
#
#   powershell -NoProfile -File scripts/shot.ps1 -Process review -Out .scratch/shots/inbox.png
#
# -Process is the executable name without .exe. Waits up to -TimeoutSec for
# the window to appear, then -SettleMs for it to finish its first frames.
param(
    [Parameter(Mandatory)] [string] $Process,
    [Parameter(Mandatory)] [string] $Out,
    [int] $TimeoutSec = 30,
    [int] $SettleMs = 1500
)
$ErrorActionPreference = 'Stop'

Add-Type -ReferencedAssemblies System.Drawing @'
using System;
using System.Drawing;
using System.Drawing.Imaging;
using System.Runtime.InteropServices;

public static class WindowShot {
    [StructLayout(LayoutKind.Sequential)]
    struct RECT { public int Left, Top, Right, Bottom; }

    [DllImport("user32.dll")] static extern bool SetProcessDpiAwarenessContext(IntPtr value);
    [DllImport("user32.dll")] static extern bool GetWindowRect(IntPtr hwnd, out RECT rect);
    [DllImport("user32.dll")] static extern bool PrintWindow(IntPtr hwnd, IntPtr hdc, uint flags);

    public static void Save(IntPtr hwnd, string path) {
        // Per-monitor v2, so the rectangle is in physical pixels.
        SetProcessDpiAwarenessContext(new IntPtr(-4));
        RECT r;
        if (!GetWindowRect(hwnd, out r)) throw new Exception("GetWindowRect failed");
        int w = r.Right - r.Left, h = r.Bottom - r.Top;
        using (var bmp = new Bitmap(w, h, PixelFormat.Format32bppArgb))
        using (var g = Graphics.FromImage(bmp)) {
            IntPtr hdc = g.GetHdc();
            // PW_RENDERFULLCONTENT (2) captures DirectX-composed content.
            bool ok = PrintWindow(hwnd, hdc, 2);
            g.ReleaseHdc(hdc);
            if (!ok) throw new Exception("PrintWindow failed");
            bmp.Save(path, ImageFormat.Png);
        }
    }
}
'@

$deadline = (Get-Date).AddSeconds($TimeoutSec)
$hwnd = [IntPtr]::Zero
while ((Get-Date) -lt $deadline) {
    $p = Get-Process -Name $Process -ErrorAction SilentlyContinue |
        Where-Object { $_.MainWindowHandle -ne [IntPtr]::Zero } | Select-Object -First 1
    if ($p) { $hwnd = $p.MainWindowHandle; break }
    Start-Sleep -Milliseconds 200
}
if ($hwnd -eq [IntPtr]::Zero) { throw "no window for process '$Process' within $TimeoutSec s" }
Start-Sleep -Milliseconds $SettleMs

$dir = Split-Path -Parent $Out
if ($dir) { New-Item -ItemType Directory -Force $dir | Out-Null }
[WindowShot]::Save($hwnd, [IO.Path]::GetFullPath($Out))
Write-Output $Out
