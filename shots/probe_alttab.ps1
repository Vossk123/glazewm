# Part 5: probe - is panel ever picked by alt-tab; full state dump per round
$code5 = @'
using System;
using System.Runtime.InteropServices;
using System.Text;
using System.Collections.Generic;

public class RT5 {
  [DllImport("user32.dll")]
  public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")]
  public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")]
  public static extern bool IsIconic(IntPtr h);
  [DllImport("user32.dll")]
  public static extern IntPtr GetWindow(IntPtr h, uint cmd);
  [DllImport("user32.dll", CharSet = CharSet.Unicode)]
  public static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")]
  public static extern int GetWindowTextLengthW(IntPtr h);
  [DllImport("user32.dll")]
  public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll", EntryPoint = "GetWindowLongPtrW")]
  public static extern long GetWindowLongPtr(IntPtr h, int idx);
  [DllImport("user32.dll", EntryPoint = "GetWindowLongW")]
  public static extern int GetWindowLong(IntPtr h, int idx);
  [DllImport("user32.dll")]
  public static extern void keybd_event(byte vk, byte scan, uint flags, IntPtr extra);
  [DllImport("user32.dll", CharSet = CharSet.Unicode)]
  public static extern IntPtr FindWindowW(string c, string t);

  const uint KEYUP = 2;

  static string Title(IntPtr h) {
    var sb = new StringBuilder(60);
    GetWindowTextW(h, sb, sb.Capacity);
    string t = sb.ToString();
    if (t.Length > 24) t = t.Substring(0, 24);
    return t.Replace("\n", " ");
  }

  public static string Dump() {
    var sb = new StringBuilder();
    IntPtr panel = FindWindowW(null, "GlazeWM Stage");
    sb.AppendLine("panel=" + panel + " vis=" + IsWindowVisible(panel) + " iconic=" + IsIconic(panel) + " ex=0x" + GetWindowLongPtr(panel, -20).ToString("X") + " style=0x" + GetWindowLongPtr(panel, -16).ToString("X"));
    sb.AppendLine("fg=" + GetForegroundWindow() + " [" + Title(GetForegroundWindow()) + "]");
    EnumWindows((h, l) => {
      if (!IsWindowVisible(h)) return true;
      if (GetWindow(h, 4) != IntPtr.Zero) return true;
      long ex = GetWindowLongPtr(h, -20);
      if ((ex & 0x80) != 0) return true;
      if (GetWindowTextLengthW(h) == 0) return true;
      string t = Title(h);
      if (t.Contains("DWM Notification") || t.Contains("Windows 输入体验") || t == "设置" || t.Contains("设置")) return true;
      sb.AppendLine("  " + h + " " + (IsIconic(h) ? "min" : "VIS") + " [" + t + "]");
      return true;
    }, IntPtr.Zero);
    return sb.ToString();
  }

  public static string AltTab() {
    keybd_event(0x12, 0, 0, IntPtr.Zero);
    keybd_event(0x09, 0, 0, IntPtr.Zero);
    keybd_event(0x09, 0, KEYUP, IntPtr.Zero);
    keybd_event(0x12, 0, KEYUP, IntPtr.Zero);
    System.Threading.Thread.Sleep(500);
    return "";
  }

  public static void HotAltShiftM() {
    keybd_event(0x12, 0, 0, IntPtr.Zero);
    keybd_event(0x10, 0, 0, IntPtr.Zero);
    keybd_event(0x4D, 0, 0, IntPtr.Zero);
    keybd_event(0x4D, 0, KEYUP, IntPtr.Zero);
    keybd_event(0x10, 0, KEYUP, IntPtr.Zero);
    keybd_event(0x12, 0, KEYUP, IntPtr.Zero);
  }
}
'@
Add-Type -TypeDefinition $code5

Write-Host "--- state before ---"; [RT5]::Dump()
foreach ($i in 1..6) {
  [void][RT5]::AltTab()
  Write-Host "--- after alttab$i ---"; [RT5]::Dump()
}
Write-Host "=== WM log tail ==="
Get-Content C:\Code\glazewm-stage-manager\shots\wm-out9.log | Select-Object -Last 20
