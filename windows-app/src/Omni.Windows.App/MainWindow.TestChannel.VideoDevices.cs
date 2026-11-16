using Microsoft.UI.Xaml.Controls;
using Omni.Windows.App.Features.Calls;

namespace Omni.Windows.App;

// Settings → Звонки → «Кодирование (отправка)» / «Декодирование (приём)» through the test channel.
// `ui.select` drives lists only; these two are drop-downs, and a choice made in a call has to be
// checked live (it rebuilds the encoder on the chosen card).
public sealed partial class MainWindow
{
    // video.encoder | video.decoder           — the rows, numbered, the chosen one marked *
    // video.encoder <row> | video.decoder <row> — pick that row exactly as a click does (mid-call too)
    private async Task<string?> TryExecuteVideoDeviceUiTestCommandAsync(string command, IReadOnlyList<string> args)
    {
        if (command is not ("video.encoder" or "video.decoder"))
        {
            return null;
        }

        ComboBox combo = command == "video.encoder" ? CallVideoEncodeDeviceComboBox : CallVideoDecodeDeviceComboBox;
        // Filled on the first visit to Settings (the probe is not run at startup).
        EnsureCallVideoDevicePickersPopulated();
        for (int attempt = 0; attempt < 100 && combo.ItemsSource is null; attempt++)
        {
            await Task.Delay(100);
        }

        if (combo.ItemsSource is not IReadOnlyList<CallVideoDeviceOption> options)
        {
            return "error: the device list did not fill";
        }

        if (args.Count == 0)
        {
            return "ok" + Environment.NewLine + string.Join(
                Environment.NewLine,
                options.Select((option, row) => $"{(ReferenceEquals(combo.SelectedItem, option) ? "*" : " ")}{row} {option.DisplayName}"));
        }

        if (!int.TryParse(args[0], out int pick) || pick < 0 || pick >= options.Count)
        {
            return $"error: {command} <row from 0 to {options.Count - 1}>";
        }

        combo.SelectedItem = options[pick];
        // The handler switches a running call over; give it the time a rebuild takes.
        await Task.Delay(2500);
        return await SettleUiTestCommandAsync();
    }
}
