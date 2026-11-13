# Part 3: alt-tab / taskbar-style path sweep regression + toggle regression
$code3 = @'
using System;
using System.Runtime.InteropServices;
using System.Text;
using System.Collections.Generic;

public class RT3 {
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
  [DllImport("dwmapi.dll")]
  public static extern int DwmGetWindowAttribute(IntPtr h, int attr, out int val, int size);
  [DllImport("user32.dll", EntryPoint = "GetWindowLongPtrW")]
  public static extern long GetWindowLongPtr(IntPtr h, int idx);
  [DllImport("user32.dll")]
  public static extern void keybd_event(byte vk, byte scan, uint flags, IntPtr extra);
  [DllImport("user32.dll")]
  public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")]
  public static extern void mouse_event(uint flags, uint dx, uint dy, uint data, IntPtr extra);

  const uint KEYUP = 2;

  static string Title(IntPtr h) {
    var sb = new StringBuilder(100);
    GetWindowTextW(h, sb, sb.Capacity);
    return sb.ToString();
  }

  static List<IntPtr> TaskbarWindows() {
    var list = new List<IntPtr>();
    EnumWindows((h, l) => {
      if (!IsWindowVisible(h)) return true;
      if (GetWindow(h, 4) != IntPtr.Zero) return true;
      long ex = GetWindowLongPtr(h, -20);
      if ((ex & 0x80) != 0) return true;
      int cloaked = 0;
      DwmGetWindowAttribute(h, 14, out cloaked, 4);
      if (cloaked != 0) return true;
      if (GetWindowTextLengthW(h) == 0) return true;
      string t = Title(h);
      if (t == "GlazeWM Stage" || t.Contains("DWM Notification")) return true;
      list.Add(h);
      return true;
    }, IntPtr.Zero);
    return list;
  }

  public static string Check() {
    int vis = 0; string visTitle = "";
    IntPtr fg = GetForegroundWindow();
    IntPtr visHwnd = IntPtr.Zero;
    foreach (var h in TaskbarWindows()) {
      if (!IsIconic(h)) { vis++; visTitle = Title(h); visHwnd = h; }
    }
    bool ok = (vis == 1) && (visHwnd == fg);
    return (ok ? "PASS" : "FAIL") + " vis=" + vis + " fgIsVis=" + (visHwnd == fg) + " win=[" + visTitle + "] fg=[" + Title(fg) + "]";
  }

  // Alt+Tab once: down alt, down tab, up tab, up alt (classic switch).
  public static string AltTab() {
    keybd_event(0x12, 0, 0, IntPtr.Zero);
    keybd_event(0x09, 0, 0, IntPtr.Zero);
    keybd_event(0x09, 0, KEYUP, IntPtr.Zero);
    keybd_event(0x12, 0, KEYUP, IntPtr.Zero);
    System.Threading.Thread.Sleep(500);
    return Check();
  }

  public static void HotToggle() {
    keybd_event(0x12, 0, 0, IntPtr.Zero);          // alt
    keybd_event(0x10, 0, 0, IntPtr.Zero);          // shift
    keybd_event(0x4D, 0, 0, IntPtr.Zero);          // m
    keybd_event(0x4D, 0, KEYUP, IntPtr.Zero);
    keybd_event(0x10, 0, KEYUP, IntPtr.Zero);
    keybd_event(0x12, 0, KEYUP, IntPtr.Zero);
  }
}
'@
Add-Type -TypeDefinition $code3

Write-Host "start: $([RT3]::Check())"
foreach ($i in 1..5) {
  Write-Host ("alttab{0}: {1}" -f $i, [RT3]::AltTab())
}
Write-Host "=== toggle off/on ==="
[RT3]::HotToggle(); Start-Sleep -Milliseconds 600
[RT3]::HotToggle(); Start-Sleep -Milliseconds 800
Write-Host ("after toggle cycle: " + [RT3]::Check())
Write-Host "=== last log lines ==="
Get-Content C:\Code\glazewm-stage-manager\shots\wm-out8.log | Select-Object -Last 12
