# Part 4: instrumented alt-tab race analysis
$code4 = @'
using System;
using System.Runtime.InteropServices;
using System.Text;
using System.Collections.Generic;

public class RT4 {
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

  const uint KEYUP = 2;

  static string Title(IntPtr h) {
    var sb = new StringBuilder(60);
    GetWindowTextW(h, sb, sb.Capacity);
    string t = sb.ToString();
    if (t.Length > 22) t = t.Substring(0, 22);
    return t.Replace("\n", " ");
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

  static string Ts() {
    return DateTime.Now.ToString("HH:mm:ss.fff");
  }

  // Alt+Tab then check repeatedly with staggered samples.
  public static string Round(int n) {
    var sb = new StringBuilder();
    sb.AppendLine(Ts() + " PRESS alttab" + n);
    keybd_event(0x12, 0, 0, IntPtr.Zero);
    keybd_event(0x09, 0, 0, IntPtr.Zero);
    keybd_event(0x09, 0, KEYUP, IntPtr.Zero);
    keybd_event(0x12, 0, KEYUP, IntPtr.Zero);
    int slept = 0;
    foreach (int mark in new int[] { 150, 350, 700 }) {
      System.Threading.Thread.Sleep(mark - slept);
      slept = mark;
      int vis = 0; string names = "";
      IntPtr fg = GetForegroundWindow();
      foreach (var h in TaskbarWindows()) {
        if (!IsIconic(h)) { vis++; names += "[" + Title(h) + "]"; }
      }
      sb.AppendLine(Ts() + " +" + mark + "ms vis=" + vis + " fg=[" + Title(fg) + "] visWins=" + names);
    }
    return sb.ToString();
  }
}
'@
Add-Type -TypeDefinition $code4

foreach ($i in 1..6) { Write-Host ([RT4]::Round($i)) }
Write-Host "=== full log tail ==="
Get-Content C:\Code\glazewm-stage-manager\shots\wm-out9.log | Select-Object -Last 45
