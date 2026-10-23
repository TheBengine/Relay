using System;
using System.Diagnostics;
using System.IO;
using System.Text;
using System.Windows.Forms;

// The portable package has one visible entry point. The client continues to own
// the optional local server, so opening Relay never starts a separate host here.
internal static class RelayLauncher
{
    internal static string QuoteArgument(string argument)
    {
        if (argument == null) throw new ArgumentNullException("argument");
        StringBuilder quoted = new StringBuilder("\"");
        int backslashes = 0;
        foreach (char character in argument)
        {
            if (character == '\\')
            {
                backslashes++;
                continue;
            }
            if (character == '"')
            {
                quoted.Append('\\', backslashes * 2 + 1);
                quoted.Append('"');
            }
            else
            {
                quoted.Append('\\', backslashes);
                quoted.Append(character);
            }
            backslashes = 0;
        }
        // Backslashes before the closing quote must themselves be escaped.
        quoted.Append('\\', backslashes * 2);
        quoted.Append('"');
        return quoted.ToString();
    }

    internal static string BuildArguments(string[] arguments)
    {
        StringBuilder commandLine = new StringBuilder();
        foreach (string argument in arguments)
        {
            if (commandLine.Length > 0) commandLine.Append(' ');
            commandLine.Append(QuoteArgument(argument));
        }
        return commandLine.ToString();
    }

    internal static ProcessStartInfo CreateStartInfo(string packageDirectory, string[] arguments)
    {
        string packageRoot = Path.GetFullPath(packageDirectory);
        return new ProcessStartInfo
        {
            FileName = Path.Combine(packageRoot, "dist", "relay.exe"),
            WorkingDirectory = packageRoot,
            Arguments = BuildArguments(arguments),
            UseShellExecute = false,
            CreateNoWindow = true
        };
    }

#if LAUNCHER_TEST
    // A console-only harness exercises this exact quoting/start-plan code. It
    // never starts Relay or displays the production launcher's error dialog.
    private static int Main(string[] arguments)
    {
        if (arguments.Length == 0) return 2;
        string[] forwarded = new string[arguments.Length - 1];
        Array.Copy(arguments, 1, forwarded, 0, forwarded.Length);
        if (arguments[0] == "--echo")
        {
            foreach (string value in forwarded) PrintEncoded(value);
            return 0;
        }
        if (arguments[0] == "--roundtrip")
        {
            ProcessStartInfo test = new ProcessStartInfo
            {
                FileName = System.Reflection.Assembly.GetExecutingAssembly().Location,
                Arguments = "--echo " + BuildArguments(forwarded),
                UseShellExecute = false,
                CreateNoWindow = true,
                RedirectStandardOutput = true,
                RedirectStandardError = true
            };
            using (Process child = Process.Start(test))
            {
                Console.Write(child.StandardOutput.ReadToEnd());
                string errors = child.StandardError.ReadToEnd();
                child.WaitForExit();
                if (errors.Length > 0) Console.Error.Write(errors);
                return child.ExitCode;
            }
        }
        if (arguments[0] == "--plan" && forwarded.Length > 0)
        {
            string[] clientArguments = new string[forwarded.Length - 1];
            Array.Copy(forwarded, 1, clientArguments, 0, clientArguments.Length);
            ProcessStartInfo plan = CreateStartInfo(forwarded[0], clientArguments);
            PrintEncoded(plan.FileName);
            PrintEncoded(plan.WorkingDirectory);
            PrintEncoded(plan.Arguments);
            Console.WriteLine(plan.UseShellExecute ? "shell" : "direct");
            Console.WriteLine(plan.CreateNoWindow ? "hidden" : "visible");
            return 0;
        }
        return 2;
    }

    private static void PrintEncoded(string value)
    {
        Console.WriteLine(Convert.ToBase64String(Encoding.UTF8.GetBytes(value)));
    }
#else
    [STAThread]
    private static int Main(string[] arguments)
    {
        ProcessStartInfo launch = CreateStartInfo(AppDomain.CurrentDomain.BaseDirectory, arguments);
        if (!File.Exists(launch.FileName))
        {
            ShowError("Relay's application file is missing.\n\nExtract the complete Relay package into one folder, then open Relay.exe again.");
            return 1;
        }
        try
        {
            // No command interpreter handles user-supplied arguments. Disposing
            // this handle does not stop the client; the launcher can exit now.
            using (Process client = Process.Start(launch))
            {
                if (client == null) throw new InvalidOperationException("Windows did not start Relay.");
            }
            return 0;
        }
        catch (Exception error)
        {
            ShowError("Relay could not start.\n\nTry extracting the complete package again.\n\n" + error.Message);
            return 1;
        }
    }

    private static void ShowError(string message)
    {
        MessageBox.Show(message, "Relay", MessageBoxButtons.OK, MessageBoxIcon.Error);
    }
#endif
}
