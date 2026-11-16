using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading.Tasks;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Omni.Windows.App.Features.Shared;

namespace Omni.Windows.App;

// «Где объявлять» (owner, 2026-10-14: «один компонент везде»): the same step editor under each of our
// publications — the contacts' beacons («Связь → Почтовые ящики»), the Omni-ID and the name
// («Приватность → Кто может вас найти»), the relay directory entry («Быть релеем для всех»). Each sits
// in a collapsed expander whose header is the plan's one-line summary.
public sealed partial class MainWindow
{
    private bool publishPlansReading;
    private string shownPublishPlans = string.Empty;

    private (string Publication, string Name)[] PublishPlanHosts =>
    [
        ("contacts", "SettingsContactsPlan"),
        ("omni_id", "SettingsOmniIdPlan"),
        ("name", "SettingsNamePlan"),
        ("relay", "SettingsRelayPlan"),
    ];

    private void InitializePublishPlans()
    {
        try
        {
            ShowPublishPlans(rustBridge.GetPublishPlans());
        }
        catch (Exception)
        {
            // Before sign-in the core has no profile; the page re-reads when shown.
        }
    }

    private async void RefreshPublishPlansAsync()
    {
        if (publishPlansReading)
        {
            return;
        }

        publishPlansReading = true;
        try
        {
            ShowPublishPlans(await Task.Run(() => rustBridge.GetPublishPlans()));
        }
        catch (Exception)
        {
            // Before sign-in the core has no profile; the page re-reads when shown.
        }
        finally
        {
            publishPlansReading = false;
        }
    }

    // The editors are rebuilt only when a plan changed: a rebuild on every visit cost tens of
    // milliseconds, and would close an editor somebody left open.
    private void ShowPublishPlans(IReadOnlyList<Core.PublishPlanView> views)
    {
        string signature = string.Join('\n', views.Select(v => string.Join('|',
            v.Publication,
            v.Plan.Auto,
            string.Join(';', v.Plan.Steps.Select(step => string.Join(',', step))),
            string.Join(',', v.AllowedKinds),
            string.Join(',', v.Unavailable),
            v.Summary)));
        if (signature == shownPublishPlans)
        {
            return;
        }

        shownPublishPlans = signature;
        foreach ((string publication, string name) in PublishPlanHosts)
        {
            Core.PublishPlanView? view = views.FirstOrDefault(v => v.Publication == publication);
            Expander expander = ResolveSectionElement<Expander>(SettingsSectionHost, name + "Expander");
            Border host = ResolveSectionElement<Border>(SettingsSectionHost, name + "Host");
            if (view is null)
            {
                expander.Visibility = Visibility.Collapsed;
                continue;
            }

            var editor = new PublishPlanEditor(view, plan => rustBridge.SetPublishPlan(publication, plan));
            editor.SummaryChanged += summary => expander.Header = PlanHeaderText(summary);
            host.Child = editor;
            expander.Header = PlanHeaderText(view.Summary);
        }
    }

    // Two lines: what this is, and the plan in words (wrapping, never cut off).
    private static StackPanel PlanHeaderText(string summary)
    {
        const string lead = "Объявление уходит ";
        string body = summary.StartsWith(lead, StringComparison.Ordinal) ? summary[lead.Length..] : summary;
        var header = new StackPanel { Spacing = 2, Margin = new Thickness(0, 8, 0, 8) };
        header.Children.Add(new TextBlock { Text = "Где объявлять" });
        header.Children.Add(new TextBlock
        {
            Text = char.ToUpper(body[0]) + body[1..].TrimEnd('.'),
            Style = (Style)Application.Current.Resources["SettingCaptionTextStyle"],
            TextWrapping = TextWrapping.WrapWholeWords,
        });
        return header;
    }
}
