// Foreground watcher for ghost tests, in its own process. A WinEvent hook on EVENT_SYSTEM_FOREGROUND reports each
// change as it happens (it wakes this thread at once); a 10 ms poll cross-checks it (no event fires when the
// foreground drops to 0). Lines are in the order things happened, timed by a high-resolution clock when logged
// (the event's own time has GetTickCount's 16 ms steps), until the stop file appears or the test (arg 3) exits:
//   <ms> <E|P> <hwnd> <class> <P parked off screen|V> <pid> <N process started after the watcher|O> <L in ghost's launched.txt|-> <tick>
// <tick> is the event's own time for E lines and GetTickCount at the poll for P lines: one clock for both.
using System; using System.IO; using System.Text; using System.Diagnostics; using System.Runtime.InteropServices;
class FgWatch {
  delegate void WinEventProc(IntPtr hook, uint ev, IntPtr hwnd, int obj, int child, uint thread, uint time);
  [DllImport("user32.dll")] static extern IntPtr SetWinEventHook(uint min, uint max, IntPtr mod, WinEventProc proc, uint pid, uint tid, uint flags);
  [DllImport("user32.dll")] static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetClassName(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] static extern uint MsgWaitForMultipleObjects(uint n, IntPtr[] h, bool all, uint ms, uint mask);
  [StructLayout(LayoutKind.Sequential)] struct MSG { public IntPtr h; public uint m; public IntPtr w, l; public uint t; public int x, y; }
  [DllImport("user32.dll")] static extern bool PeekMessage(out MSG m, IntPtr h, uint a, uint b, uint r);
  [DllImport("user32.dll")] static extern bool TranslateMessage(ref MSG m);
  [DllImport("user32.dll")] static extern IntPtr DispatchMessage(ref MSG m);
  [DllImport("kernel32.dll")] static extern uint GetTickCount();
  [StructLayout(LayoutKind.Sequential)] struct RC { public int L, T, R, B; }
  [DllImport("user32.dll")] static extern bool GetWindowRect(IntPtr h, out RC r);
  [DllImport("user32.dll")] static extern IntPtr GetAncestor(IntPtr h, uint f);
  [DllImport("user32.dll")] static extern int GetSystemMetrics(int i);
  static Stopwatch clock = Stopwatch.StartNew();
  static StreamWriter w; static IntPtr last = new IntPtr(-1); static DateTime t0 = DateTime.Now; static string launched;
  static WinEventProc keep;
  static string Vis(IntPtr h) {
    RC r; if (h == IntPtr.Zero || !GetWindowRect(GetAncestor(h, 2), out r)) return "V";
    int l = GetSystemMetrics(76), t = GetSystemMetrics(77), rr = l + GetSystemMetrics(78), b = t + GetSystemMetrics(79);
    return (r.L < rr && r.R > l && r.T < b && r.B > t) ? "V" : "P";
  }
  // Every event is logged (the one that hands the foreground back included); a poll only when it sees a change.
  static void Log(char src, IntPtr h, uint tick) {
    if (src == 'P' && h == last) return;
    last = h;
    var c = new StringBuilder(128); GetClassName(h, c, 128);
    uint pid; GetWindowThreadProcessId(h, out pid);
    string fresh = "O";
    try { if (pid != 0 && Process.GetProcessById((int)pid).StartTime > t0) fresh = "N"; } catch { }
    string lst = "-";
    try {
      foreach (var line in File.ReadAllLines(launched)) {
        var p = line.Split(' ');
        if (p.Length >= 2 && (p[0] == h.ToInt64().ToString() || p[1] == pid.ToString())) { lst = "L"; break; }
      }
    } catch { }
    w.WriteLine(clock.Elapsed.TotalMilliseconds.ToString("F1", System.Globalization.CultureInfo.InvariantCulture) + " " + src + " " + h.ToInt64() + " " + (c.Length > 0 ? c.ToString() : "-") + " " + Vis(h) + " " + pid + " " + fresh + " " + lst + " " + tick);
  }
  static void Main(string[] a) {
    w = new StreamWriter(a[0], false) { AutoFlush = true };
    launched = Path.Combine(Environment.GetEnvironmentVariable("LOCALAPPDATA") ?? "", "ghost", "launched.txt");
    keep = (hook, ev, h, obj, child, thread, time) => Log('E', h, time);
    if (SetWinEventHook(3, 3, IntPtr.Zero, keep, 0, 0, 0) == IntPtr.Zero) { w.WriteLine("HOOK FAILED"); return; }
    Log('P', GetForegroundWindow(), GetTickCount());
    File.WriteAllText(a[0] + ".ready", "");
    Process parent = null; try { parent = Process.GetProcessById(int.Parse(a[2])); } catch { }
    while (!File.Exists(a[1]) && (parent == null || !parent.HasExited)) {
      MsgWaitForMultipleObjects(0, null, false, 10, 0x4FF);
      MSG m;
      while (PeekMessage(out m, IntPtr.Zero, 0, 0, 1)) { TranslateMessage(ref m); DispatchMessage(ref m); }
      Log('P', GetForegroundWindow(), GetTickCount());
    }
    w.WriteLine(clock.Elapsed.TotalMilliseconds.ToString("F1", System.Globalization.CultureInfo.InvariantCulture) + " END " + GetTickCount());
  }
}
