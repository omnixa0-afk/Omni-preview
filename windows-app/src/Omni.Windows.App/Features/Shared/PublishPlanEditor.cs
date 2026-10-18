using System;
using System.Collections.Generic;
using System.Linq;
using Microsoft.UI.Text;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using Omni.Windows.App.Core;
using Windows.ApplicationModel.DataTransfer;

namespace Omni.Windows.App.Features.Shared;

/// <summary>
/// «Где объявлять» — one editor for every publication (owner, 2026-10-14; docs/decisions/
/// public-relay-directory.md §10). Steps top to bottom in one frame: the blocks of a step go at once,
/// the step below only if none of them took it. A block is a pill — ⠿ name · how often ▾ · ⋯ — dragged
/// by its dots onto a step (at once with it), onto the line between steps (a step of its own) or onto
/// «Не используются»; the ⋯ menu does the same without a mouse. Kinds the publication cannot use stay
/// visible there, switched off, with the reason. «Авто» is the publication's preset; any change makes
/// it «Вручную», «Сбросить к «Авто»» goes back. The core checks every plan and answers with it as
/// stored; the editor redraws from that answer.
/// </summary>
internal sealed class PublishPlanEditor : UserControl
{
    private static readonly (int Secs, string Short, string Long)[] Frequencies =
    [
        (5 * 60, "5 мин", "Каждые 5 минут"),
        (15 * 60, "15 мин", "Каждые 15 минут"),
        (30 * 60, "30 мин", "Каждые 30 минут"),
        (60 * 60, "1 ч", "Раз в час"),
        (12 * 60 * 60, "12 ч", "Раз в 12 часов"),
    ];

    private const int NtfyMinEverySecs = 15 * 60;

    private readonly StackPanel root = new() { Spacing = 10 };
    private readonly Func<PublishPlan, PublishPlanView> save;
    private PublishPlanView view;
    private string? error;

    public PublishPlanEditor(PublishPlanView view, Func<PublishPlan, PublishPlanView> save)
    {
        this.view = view;
        this.save = save;
        Content = root;
        Render();
    }

    /// <summary>The plan in one line — the expander around the editor shows it.</summary>
    public event Action<string>? SummaryChanged;

    private List<List<PublishBlock>> CopySteps() => view.Plan.Steps.Select(step => step.ToList()).ToList();

    private void Apply(List<List<PublishBlock>> steps)
    {
        steps.RemoveAll(step => step.Count == 0);
        Commit(new PublishPlan(false, steps.Select(step => (IReadOnlyList<PublishBlock>)step).ToList()));
    }

    private void Commit(PublishPlan plan)
    {
        try
        {
            view = save(plan);
            error = null;
            SummaryChanged?.Invoke(view.Summary);
        }
        catch (Exception ex)
        {
            error = ex.Message;
        }

        Render();
    }

    private void Render()
    {
        root.Children.Clear();
        root.Children.Add(BuildModeRow());
        root.Children.Add(BuildStepsFrame());
        root.Children.Add(BuildUnusedRow());
        if (error is not null)
        {
            root.Children.Add(new TextBlock { Text = $"Не сохранено: {error}", Style = Caption(), TextWrapping = TextWrapping.WrapWholeWords });
        }
    }

    // ── «Авто / Вручную» ────────────────────────────────────────────────────────────────────────────

    private UIElement BuildModeRow()
    {
        var grid = new Grid();
        grid.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        grid.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        var modes = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 20 };
        var auto = new RadioButton { Content = "Авто", MinWidth = 0, IsChecked = view.Plan.Auto, GroupName = $"plan-{view.Publication}" };
        var manual = new RadioButton { Content = "Вручную", MinWidth = 0, IsChecked = !view.Plan.Auto, GroupName = $"plan-{view.Publication}" };
        auto.Checked += (_, _) =>
        {
            if (!view.Plan.Auto)
            {
                Commit(new PublishPlan(true, []));
            }
        };
        // The preset as it is, now editable.
        manual.Checked += (_, _) =>
        {
            if (view.Plan.Auto)
            {
                Commit(view.Plan with { Auto = false });
            }
        };
        modes.Children.Add(auto);
        modes.Children.Add(manual);
        grid.Children.Add(modes);
        if (!view.Plan.Auto)
        {
            var reset = new HyperlinkButton { Content = "Сбросить к «Авто»", VerticalAlignment = VerticalAlignment.Center };
            reset.Click += (_, _) => Commit(new PublishPlan(true, []));
            Grid.SetColumn(reset, 1);
            grid.Children.Add(reset);
        }

        return grid;
    }

    // ── The steps ───────────────────────────────────────────────────────────────────────────────────

    private UIElement BuildStepsFrame()
    {
        var stack = new StackPanel();
        List<List<PublishBlock>> steps = CopySteps();
        for (int n = 0; n < steps.Count; n++)
        {
            if (n > 0)
            {
                stack.Children.Add(BuildDivider(n, steps[n - 1].Count > 1 ? "если не приняли" : "если не принял"));
            }

            stack.Children.Add(BuildStepRow(n, steps[n]));
        }

        stack.Children.Add(BuildDivider(steps.Count, "перетащите сюда — отдельным шагом в конце"));
        return new Border
        {
            Child = stack,
            CornerRadius = new CornerRadius(8),
            BorderThickness = new Thickness(1),
            BorderBrush = Brush("OmniStrokeBrush"),
            Background = Brush("OmniBgAppBrush"),
        };
    }

    private UIElement BuildStepRow(int index, List<PublishBlock> step)
    {
        var grid = new Grid { ColumnSpacing = 12, Padding = new Thickness(12, 10, 12, 10) };
        grid.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        grid.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        var badge = new Border
        {
            Width = 24,
            Height = 24,
            CornerRadius = new CornerRadius(12),
            Background = Brush("OmniAccentSoftBrush"),
            VerticalAlignment = VerticalAlignment.Center,
            Child = new TextBlock
            {
                Text = (index + 1).ToString(),
                FontSize = 12,
                FontWeight = FontWeights.SemiBold,
                Foreground = Brush("OmniAccentBrush"),
                HorizontalAlignment = HorizontalAlignment.Center,
                VerticalAlignment = VerticalAlignment.Center,
            },
        };
        ToolTipService.SetToolTip(badge, index == 0 ? "Шаг 1 — сразу" : $"Шаг {index + 1} — если шаги выше не приняли");
        grid.Children.Add(badge);

        var chips = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8, VerticalAlignment = VerticalAlignment.Center };
        foreach (PublishBlock block in step)
        {
            chips.Children.Add(BuildChip(block, index));
        }

        Grid.SetColumn(chips, 1);
        grid.Children.Add(chips);
        // Onto a step: at once with its blocks.
        var row = new Border { Child = grid, Background = Transparent(), AllowDrop = true };
        WireDrop(row, kind => MoveInto(kind, index), on => row.Background = on ? Brush("OmniAccentSoftBrush") : Transparent());
        return row;
    }

    // Between steps (and after the last): a step of its own at `position`.
    private UIElement BuildDivider(int position, string label)
    {
        var line = new Border { Height = 1, Background = Brush("OmniStrokeBrush"), VerticalAlignment = VerticalAlignment.Center };
        var text = new TextBlock
        {
            Text = position < view.Plan.Steps.Count ? $"↓ {label}" : label,
            FontSize = 12,
            Foreground = Brush("OmniMutedTextBrush"),
            Margin = new Thickness(10, 0, 10, 0),
            VerticalAlignment = VerticalAlignment.Center,
        };
        var grid = new Grid { Padding = new Thickness(12, 2, 12, 2), MinHeight = 22 };
        grid.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(24) });
        grid.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        grid.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        Grid.SetColumn(text, 1);
        Grid.SetColumn(line, 2);
        grid.Children.Add(text);
        grid.Children.Add(line);
        var target = new Border { Child = grid, Background = Transparent(), AllowDrop = true };
        WireDrop(target, kind => MoveAsStep(kind, position), on =>
        {
            line.Background = on ? Brush("OmniAccentBrush") : Brush("OmniStrokeBrush");
            line.Height = on ? 2 : 1;
            text.Foreground = on ? Brush("OmniAccentBrush") : Brush("OmniMutedTextBrush");
        });
        return target;
    }

    private UIElement BuildChip(PublishBlock block, int stepIndex)
    {
        var content = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 2, VerticalAlignment = VerticalAlignment.Center };
        content.Children.Add(Handle());
        content.Children.Add(new TextBlock
        {
            Text = KindName(block.Kind),
            FontWeight = FontWeights.SemiBold,
            VerticalAlignment = VerticalAlignment.Center,
            Margin = new Thickness(4, 0, 2, 0),
        });
        content.Children.Add(BuildFrequencyButton(block));
        if (block.Kind == "ntfy")
        {
            var warn = new TextBlock { Text = "⚠", Foreground = Brush("OmniMutedTextBrush"), VerticalAlignment = VerticalAlignment.Center, Margin = new Thickness(2, 0, 2, 0) };
            ToolTipService.SetToolTip(warn, "ntfy.sh: 250 сообщений в сутки на ваш IP — один лимит на всех за роутером.");
            content.Children.Add(warn);
        }

        var more = SmallButton("⋯");
        more.Flyout = BuildBlockMenu(block.Kind, stepIndex);
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(more, $"Действия с блоком {KindName(block.Kind)}");
        content.Children.Add(more);
        var chip = Pill(content, dimmed: false);
        chip.CanDrag = true;
        chip.DragStarting += (_, args) => args.Data.SetText(block.Kind);
        return chip;
    }

    private Button BuildFrequencyButton(PublishBlock block)
    {
        string current = Frequencies.FirstOrDefault(f => f.Secs == block.EverySecs).Short ?? $"{block.EverySecs / 60} мин";
        var button = SmallButton($"{current} ▾");
        button.Foreground = Brush("OmniMutedTextBrush");
        ToolTipService.SetToolTip(button, "Как часто публиковать");
        var menu = new MenuFlyout();
        foreach ((int secs, _, string label) in Frequencies)
        {
            if (block.Kind == "ntfy" && secs < NtfyMinEverySecs)
            {
                continue; // its daily quota
            }

            var item = new RadioMenuFlyoutItem { Text = label, IsChecked = secs == block.EverySecs, GroupName = $"every-{view.Publication}-{block.Kind}" };
            item.Click += (_, _) => SetEvery(block.Kind, secs);
            menu.Items.Add(item);
        }

        button.Flyout = menu;
        return button;
    }

    // ── «Не используются» ───────────────────────────────────────────────────────────────────────────

    private UIElement BuildUnusedRow()
    {
        IEnumerable<string> used = view.Plan.Steps.SelectMany(step => step).Select(block => block.Kind);
        List<string> off = view.AllowedKinds.Except(used).ToList();

        var chips = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8, VerticalAlignment = VerticalAlignment.Center };
        chips.Children.Add(new TextBlock { Text = "Не используются:", Style = Caption(), VerticalAlignment = VerticalAlignment.Center, Margin = new Thickness(0, 0, 4, 0) });
        foreach (string kind in off)
        {
            chips.Children.Add(BuildOffChip(kind));
        }

        foreach (UnavailableKind unavailable in view.Unavailable)
        {
            var content = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 6 };
            content.Children.Add(new TextBlock { Text = KindName(unavailable.Kind), VerticalAlignment = VerticalAlignment.Center });
            content.Children.Add(new TextBlock { Text = "недоступно", FontSize = 12, VerticalAlignment = VerticalAlignment.Center });
            Border pill = Pill(content, dimmed: true);
            pill.Opacity = 0.55;
            ToolTipService.SetToolTip(pill, unavailable.Reason);
            chips.Children.Add(pill);
        }

        if (off.Count == 0 && view.Unavailable.Count == 0)
        {
            chips.Children.Add(new TextBlock { Text = "— перетащите сюда блок, чтобы выключить", Style = Caption(), VerticalAlignment = VerticalAlignment.Center });
        }

        var stack = new StackPanel { Spacing = 4 };
        var row = new Border { Child = chips, Padding = new Thickness(12, 6, 12, 6), CornerRadius = new CornerRadius(8), Background = Transparent(), AllowDrop = true };
        WireDrop(row, Remove, on => row.Background = on ? Brush("OmniAccentSoftBrush") : Transparent());
        stack.Children.Add(row);
        // The reasons in words too: a tooltip alone is invisible on touch.
        foreach (UnavailableKind unavailable in view.Unavailable)
        {
            stack.Children.Add(new TextBlock
            {
                Text = $"{KindName(unavailable.Kind)} — {char.ToLower(unavailable.Reason[0])}{unavailable.Reason[1..]}",
                Style = Caption(),
                TextWrapping = TextWrapping.WrapWholeWords,
                Margin = new Thickness(12, 0, 12, 0),
            });
        }

        return stack;
    }

    private UIElement BuildOffChip(string kind)
    {
        var content = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 2 };
        content.Children.Add(Handle());
        content.Children.Add(new TextBlock { Text = KindName(kind), VerticalAlignment = VerticalAlignment.Center, Margin = new Thickness(4, 0, 2, 0) });
        var more = SmallButton("⋯");
        var menu = new MenuFlyout();
        var back = new MenuFlyoutItem { Text = "Включить отдельным шагом в конце" };
        back.Click += (_, _) => MoveAsStep(kind, int.MaxValue);
        menu.Items.Add(back);
        if (view.Plan.Steps.Count > 0)
        {
            var together = new MenuFlyoutItem { Text = "Включить вместе с шагом 1" };
            together.Click += (_, _) => MoveInto(kind, 0);
            menu.Items.Add(together);
        }

        more.Flyout = menu;
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(more, $"Действия с блоком {KindName(kind)}");
        content.Children.Add(more);
        Border chip = Pill(content, dimmed: true);
        chip.CanDrag = true;
        chip.DragStarting += (_, args) => args.Data.SetText(kind);
        return chip;
    }

    private MenuFlyout BuildBlockMenu(string kind, int stepIndex)
    {
        int stepCount = view.Plan.Steps.Count;
        bool alone = view.Plan.Steps[stepIndex].Count == 1;
        var menu = new MenuFlyout();
        void Add(string text, bool enabled, Action action)
        {
            var item = new MenuFlyoutItem { Text = text, IsEnabled = enabled };
            item.Click += (_, _) => action();
            menu.Items.Add(item);
        }

        Add("Вместе с шагом выше", stepIndex > 0, () => MoveInto(kind, stepIndex - 1));
        Add("Отдельным шагом", !alone, () => MoveAsStep(kind, stepIndex + 1));
        Add("Выше", stepIndex > 0 || !alone, () => MoveAsStep(kind, stepIndex));
        Add("Ниже", stepIndex < stepCount - 1 || !alone, () => MoveAsStep(kind, stepIndex + 2));
        menu.Items.Add(new MenuFlyoutSeparator());
        Add("Не использовать", true, () => Remove(kind));
        return menu;
    }

    // ── What the menu and the drops do ──────────────────────────────────────────────────────────────

    /// <summary>
    /// What the ⋯ menu does, by name — for the test channel (`plan.do`), which cannot click:
    /// `with-above` · `alone` · `up` · `down` · `off` · `on-end` · `on-first` · `auto` (the kind is ignored).
    /// </summary>
    internal string Perform(string kind, string action)
    {
        int stepIndex = view.Plan.Steps.ToList().FindIndex(step => step.Any(block => block.Kind == kind));
        switch (action)
        {
            case "with-above" when stepIndex > 0: MoveInto(kind, stepIndex - 1); break;
            case "alone" when stepIndex >= 0: MoveAsStep(kind, stepIndex + 1); break;
            case "up" when stepIndex >= 0: MoveAsStep(kind, stepIndex); break;
            case "down" when stepIndex >= 0: MoveAsStep(kind, stepIndex + 2); break;
            case "off": Remove(kind); break;
            case "on-end": MoveAsStep(kind, int.MaxValue); break;
            case "on-first": MoveInto(kind, 0); break;
            case "auto": Commit(new PublishPlan(true, [])); break;
            default: return $"error: cannot {action} {kind}";
        }

        return error is null ? $"ok {view.Summary}" : $"error: {error}";
    }

    private void SetEvery(string kind, int secs)
    {
        List<List<PublishBlock>> steps = CopySteps();
        foreach (List<PublishBlock> step in steps)
        {
            int i = step.FindIndex(b => b.Kind == kind);
            if (i >= 0)
            {
                step[i] = step[i] with { EverySecs = secs };
            }
        }

        Apply(steps);
    }

    // Take the block out of wherever it is (or make it, out of «Не используются»), then place it.
    private (List<List<PublishBlock>> Steps, PublishBlock Block) TakeOut(string kind)
    {
        List<List<PublishBlock>> steps = CopySteps();
        PublishBlock? block = null;
        foreach (List<PublishBlock> step in steps)
        {
            int i = step.FindIndex(b => b.Kind == kind);
            if (i >= 0)
            {
                block = step[i];
                step.RemoveAt(i);
            }
        }

        // Out of «Не используются»: once a window, the quota-safe frequency.
        return (steps, block ?? new PublishBlock(kind, 12 * 60 * 60));
    }

    private void MoveInto(string kind, int stepIndex)
    {
        List<List<PublishBlock>> before = CopySteps();
        (List<List<PublishBlock>> steps, PublishBlock block) = TakeOut(kind);
        // The index is of the row as drawn; taking the block out may have emptied a row above it.
        List<PublishBlock> target = before.ElementAtOrDefault(stepIndex) is { } drawn
            ? steps.FirstOrDefault(step => step.Any(b => drawn.Any(d => d.Kind == b.Kind))) ?? NewStepAt(steps, stepIndex)
            : NewStepAt(steps, steps.Count);
        target.Add(block);
        Apply(steps);
    }

    private void MoveAsStep(string kind, int position)
    {
        List<List<PublishBlock>> before = CopySteps();
        (List<List<PublishBlock>> steps, PublishBlock block) = TakeOut(kind);
        // Count the drawn rows above `position` that still have blocks after the take-out.
        int index = 0;
        for (int n = 0; n < Math.Min(position, before.Count); n++)
        {
            if (before[n].Any(b => b.Kind != kind))
            {
                index++;
            }
        }

        steps.RemoveAll(step => step.Count == 0);
        steps.Insert(Math.Min(index, steps.Count), [block]);
        Apply(steps);
    }

    private static List<PublishBlock> NewStepAt(List<List<PublishBlock>> steps, int index)
    {
        var step = new List<PublishBlock>();
        steps.Insert(Math.Min(index, steps.Count), step);
        return step;
    }

    private void Remove(string kind)
    {
        (List<List<PublishBlock>> steps, _) = TakeOut(kind);
        Apply(steps);
    }

    // ── Pieces ──────────────────────────────────────────────────────────────────────────────────────

    // `highlight(true)` while a block is held over the target, `false` when it leaves or lands.
    private static void WireDrop(UIElement target, Action<string> onDrop, Action<bool> highlight)
    {
        target.DragEnter += (_, _) => highlight(true);
        target.DragLeave += (_, _) => highlight(false);
        target.DragOver += (_, e) =>
        {
            e.AcceptedOperation = DataPackageOperation.Move;
            e.DragUIOverride.IsCaptionVisible = false;
        };
        target.Drop += async (_, e) =>
        {
            highlight(false);
            if (e.DataView.Contains(StandardDataFormats.Text))
            {
                onDrop(await e.DataView.GetTextAsync());
            }
        };
    }

    private static TextBlock Handle() => new()
    {
        Text = "⠿",
        FontSize = 14,
        Foreground = Brush("OmniMutedTextBrush"),
        VerticalAlignment = VerticalAlignment.Center,
        Margin = new Thickness(0, 0, 0, 1),
    };

    private static Border Pill(UIElement content, bool dimmed) => new()
    {
        Child = content,
        Padding = new Thickness(10, 3, 4, 3),
        CornerRadius = new CornerRadius(16),
        BorderThickness = new Thickness(1),
        BorderBrush = Brush(dimmed ? "OmniStrokeBrush" : "OmniSubtleStrokeBrush"),
        Background = dimmed ? Transparent() : Brush("OmniBgElevatedBrush"),
    };

    // Small and quiet: the default button style is a full-size command.
    private static Button SmallButton(string text) => new()
    {
        Content = text,
        MinWidth = 0,
        MinHeight = 0,
        Padding = new Thickness(6, 2, 6, 3),
        BorderThickness = new Thickness(0),
        Background = Transparent(),
        VerticalAlignment = VerticalAlignment.Center,
    };

    private static string KindName(string kind) => kind switch
    {
        "nostr" => "Nostr",
        "dht" => "Mainline",
        "ntfy" => "ntfy",
        _ => kind,
    };

    private static Style Caption() => (Style)Application.Current.Resources["SettingCaptionTextStyle"];

    // The indexer, as SettingRow does: it also looks into the theme dictionaries (TryGetValue does not).
    private static Brush Brush(string key) => (Brush)Application.Current.Resources[key];

    private static Brush Transparent() => new SolidColorBrush(Microsoft.UI.Colors.Transparent);
}
