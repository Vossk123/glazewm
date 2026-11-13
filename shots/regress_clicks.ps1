# Part 2: repeated card clicks, invariant = exactly one visible+focused window
$code2 = @'
using System;
using System.Runtime.InteropServices;
using System.Text;
using System.Collections.Generic;

public class RT2 {
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
  public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")]
  public static extern void mouse_event(uint flags, uint dx, uint dy, uint data, IntPtr extra);

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

  // Invariant check: exactly one non-minimized window, and it is foreground.
  public static string Check() {
    int vis = 0; string visTitle = ""; IntPtr fg = GetForegroundWindow();
    IntPtr visHwnd = IntPtr.Zero;
    foreach (var h in TaskbarWindows()) {
      if (!IsIconic(h)) { vis++; visTitle = Title(h); visHwnd = h; }
    }
    bool ok = (vis == 1) && (visHwnd == fg);
    return (ok ? "PASS" : "FAIL") + " vis=" + vis + " fgIsVis=" + (visHwnd == fg) + " win=[" + visTitle + "]";
  }

  public static string ClickCard0() {
    SetCursorPos(2432, 98);
    System.Threading.Thread.Sleep(80);
    mouse_event(0x2, 0, 0, 0, IntPtr.Zero);
    mouse_event(0x4, 0, 0, 0, IntPtr.Zero);
    System.Threading.Thread.Sleep(450);
    return Check();
  }
}
'@
Add-Type -TypeDefinition $code2

foreach ($i in 1..6) {
  Write-Host ("click{0}: {1}" -f $i, [RT2]::ClickCard0())
}
Write-Host "=== WM log (stage lines, last 30) ==="
Get-Content shots\wm-out8.log | Select-String -Pattern 'stage|Stage|card|Card|ERROR|panic' | Select-Object -Last 30
