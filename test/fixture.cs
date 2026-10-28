// Throwaway window for ghost tests, so tests never touch the user's real apps (Notepad session restore holds unsaved tabs).
using System;
using System.Threading;
using System.Windows.Forms;

class Fixture {
    [STAThread]
    static void Main(string[] args) {
        var f = new Form();
        f.Text = args.Length > 0 ? args[0] : "ghost-fixture";
        f.Width = 420; f.Height = 300;
        var box = new TextBox();
        box.Multiline = true; box.AccessibleName = "Body"; box.Left = 10; box.Top = 10; box.Width = 380; box.Height = 120;        var status = new Label();
        status.Text = "idle"; status.Left = 10; status.Top = 210; status.Width = 380;
        var btn = new Button();
        btn.Text = "Settings"; btn.Left = 10; btn.Top = 140; btn.Width = 100;
        btn.Click += delegate { status.Text = "clicked"; };
        var hold = new CheckBox();
        hold.Text = "Hold"; hold.Left = 120; hold.Top = 140; hold.Width = 80;
        // Freeze blocks the UI thread so tests can check ghost against a hung app.
        var freeze = new Button();
        freeze.Text = "Freeze"; freeze.Left = 210; freeze.Top = 140; freeze.Width = 80;
        freeze.Click += delegate { Thread.Sleep(4000); };
        // Locked is disabled: a user can't press it, so ghost must refuse. Noop does nothing visible.
        var locked = new Button();
        locked.Text = "Locked"; locked.Left = 300; locked.Top = 140; locked.Width = 80; locked.Enabled = false;
        locked.Click += delegate { status.Text = "locked-fired"; };
        var noop = new Button();
        noop.Text = "Noop"; noop.Left = 10; noop.Top = 175; noop.Width = 80;
        // Ask opens a modal dialog owned by the form, for close/dialog tests.
        var ask = new Button();
        ask.Text = "Ask"; ask.Left = 100; ask.Top = 175; ask.Width = 80;
        ask.Click += delegate { MessageBox.Show(f, "Sure?", "Confirm"); status.Text = "answered"; };
        f.Controls.Add(ask);
        f.Controls.Add(box); f.Controls.Add(btn); f.Controls.Add(hold); f.Controls.Add(freeze); f.Controls.Add(locked); f.Controls.Add(noop); f.Controls.Add(status);
        // Maximizing opens an owned, non-modal "Tip" window, to test that ghost never leaves the owner disabled.
        Form tip = null;
        f.Resize += delegate {
            if (f.WindowState == FormWindowState.Maximized && tip == null) {
                tip = new Form(); tip.Text = "Tip"; tip.Width = 200; tip.Height = 100; tip.ShowInTaskbar = false; tip.Show(f);
            }
        };
        Application.Run(f);
    }
}
