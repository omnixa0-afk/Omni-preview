using System;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Threading.Tasks;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Omni.Windows.App.Core;
using Omni.Windows.App.Presentation;

namespace Omni.Windows.App.Features.Workspaces;

public sealed partial class WorkspacesSectionControl
{
    /// «Убрать папку с этого устройства»: the folder and its files stay on disk as they are and stop
    /// following the project; the project stays in the list. `ask` false: the test channel, which
    /// cannot press a dialog's button.
    internal async Task UnbindFolderAsync(WorkspaceRepoState repo, bool ask = true)
    {
        if (bridge is null || groupId is null || repo.WorkPath is not { } folder) return;
        RustBridge core = bridge; string id = groupId;
        if (ask)
        {
            StackPanel body = new() { Spacing = 10, MaxWidth = 520 };
            body.Children.Add(Body($"Папка {folder} и её файлы останутся на диске как есть, но перестанут обновляться. Проект останется в списке: файлы можно скачать по одному или снова забрать в папку."));
            if (repo.ChangedCount > 0)
                body.Children.Add(Body($"В папке есть несохранённые изменения ({repo.ChangedCount}) — в проект они не попадут, пока папка не вернётся и вы их не сохраните."));
            if (repo.Own is { State: not "rejected" })
                body.Children.Add(Body("Ваша версия ждёт принятия — она уже сохранена и останется в проекте."));
            ContentDialog confirm = new()
            {
                XamlRoot = XamlRoot, RequestedTheme = MainWindowPresentation.ActiveTheme,
                Title = $"Убрать папку «{repo.Repo}» с этого компьютера?", Content = body,
                PrimaryButtonText = "Убрать", CloseButtonText = "Отмена", DefaultButton = ContentDialogButton.Close,
            };
            if (await confirm.ShowAsync() != ContentDialogResult.Primary) return;
        }
        try
        {
            await Task.Run(() => core.WorkspaceFiles(new("unbind", id, repo.Repo)));
            SetStatus($"Папка убрана: файлы остались в {folder}.");
        }
        catch (Exception ex) { SetStatus($"Не удалось убрать папку: {ex.Message}", StatusKind.Failed); }
        signature = ""; await LoadAsync();
    }

    /// «Вернуть к общей»: our version that waits or was turned down leaves our line, the folder follows the
    /// shared version. `ask` false: the test channel, which cannot press a dialog's button.
    internal async Task BackToSharedAsync(WorkspaceRepoState repo, bool ask = true)
    {
        if (bridge is null || groupId is null || repo.Own is not { } own) return;
        RustBridge core = bridge; string id = groupId;
        if (ask)
        {
            string versions = own.Count > 1 ? $"Ваши версии ({own.Count}), последняя «{Shorten(own.Commit.Subject, 60)}»," : $"Ваша версия «{Shorten(own.Commit.Subject, 60)}»";
            StackPanel body = new() { Spacing = 10, MaxWidth = 520 };
            body.Children.Add(Body($"{versions} уйдут из проекта{(own.State == "waiting" ? " и не попадут на принятие" : "")}. Файлы в папке станут как в общей версии {repo.Version}."));
            body.Children.Add(Body(repo.ChangedCount > 0
                ? $"Несохранённые изменения в папке ({repo.ChangedCount}) не трогаю: пока они есть, эти файлы останутся как есть."
                : "Нужный файл можно заранее скопировать из папки или открыть его версию в «Файлах»."));
            ContentDialog confirm = new()
            {
                XamlRoot = XamlRoot, RequestedTheme = MainWindowPresentation.ActiveTheme,
                Title = $"Вернуть «{repo.Repo}» к общей версии?", Content = body,
                PrimaryButtonText = "Вернуть", CloseButtonText = "Отмена", DefaultButton = ContentDialogButton.Close,
            };
            if (await confirm.ShowAsync() != ContentDialogResult.Primary) return;
        }
        try
        {
            await Task.Run(() => core.WorkspaceFiles(new("back_to_shared", id, repo.Repo)));
            SetStatus($"«{repo.Repo}» возвращается к общей версии {repo.Version}.");
        }
        catch (Exception ex) { SetStatus($"Не удалось вернуть к общей: {WorkspaceErrorText.ForCoreMessage(ex.Message)}", StatusKind.Failed); }
        signature = ""; await LoadAsync();
    }

    /// «История» of a files project, a tab of its page (docs/decisions/workspace-files-storage.md,
    /// stage 3): the accepted versions on the left, the chosen one on the right — an admin or a
    /// custodian releases a revision (A, B, …) and keeps a version for good; anyone brings an old
    /// version back as a new one, accepted as any save. Opened on the current version, or on the
    /// revision that waits for нормоконтроль when the page was opened for it.
    private async Task FillHistoryAsync(string repo, Grid host, bool pendingFirst, Func<bool> live, Action<string, string> showFiles, string? chosenBefore = null)
    {
        if (bridge is null || groupId is null) return;
        RustBridge core = bridge; string id = groupId;
        host.Children.Clear();
        host.ColumnDefinitions.Clear();
        host.ColumnSpacing = 24;
        // The same width as the files' list: the chosen one's details start on the same line.
        ColumnDefinition listColumn = new() { Width = new GridLength(Features.Shared.ColumnWidth.PageList.Width) };
        host.ColumnDefinitions.Add(listColumn);
        void FollowWidth()
        {
            if (!live()) { Features.Shared.ColumnWidth.PageList.Changed -= FollowWidth; return; }
            listColumn.Width = new GridLength(Features.Shared.ColumnWidth.PageList.Width);
        }
        Features.Shared.ColumnWidth.PageList.Changed += FollowWidth;
        host.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        ListView list = new() { SelectionMode = ListViewSelectionMode.Single };
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(list, $"Версии проекта {repo}");
        host.Children.Add(list);
        host.Children.Add(new Features.Shared.RailSplitter { Source = Features.Shared.ColumnWidth.PageList });
        StackPanel actions = new() { Spacing = 10, MaxWidth = 640, HorizontalAlignment = HorizontalAlignment.Left };
        ScrollViewer right = new() { Content = actions, VerticalScrollBarVisibility = ScrollBarVisibility.Auto };
        Grid.SetColumn(right, 1);
        host.Children.Add(right);
        WorkspaceFilesDetails? view = null;
        string? chosen = chosenBefore;
        bool busy = false;
        async Task Act(WorkspaceFilesRequest request, string done)
        {
            if (busy) return;
            busy = true;
            try { view = await Task.Run(() => core.WorkspaceFiles(request)); if (!live()) return; SetStatus(done); Draw(); }
            catch (Exception ex) { if (live()) SetStatus(WorkspaceErrorText.ForCoreMessage(ex.Message), StatusKind.Failed); }
            finally { busy = false; }
        }
        void Draw()
        {
            if (view is null) return;
            List<WorkspaceHistoryEntry> entries = view.History ?? new();
            chosen ??= (pendingFirst ? entries.FirstOrDefault(e => e.RevisionPending) : null)?.Oid ?? entries.FirstOrDefault()?.Oid;
            list.Items.Clear();
            foreach (WorkspaceHistoryEntry entry in entries)
            {
                ListViewItem item = new() { Content = HistoryRow(entry), Tag = entry, HorizontalContentAlignment = HorizontalAlignment.Stretch };
                list.Items.Add(item);
                if (entry.Oid == chosen) list.SelectedItem = item;
            }
            if (list.SelectedItem is ListViewItem selected) list.ScrollIntoView(selected);
            DrawChosen();
        }
        void DrawChosen()
        {
            actions.Children.Clear();
            if (view is null || (list.SelectedItem as ListViewItem)?.Tag is not WorkspaceHistoryEntry entry) return;
            actions.Children.Add(new TextBlock { Text = $"в.{entry.Number} · «{entry.Subject}»", Style = CardTitle, TextWrapping = TextWrapping.Wrap });
            actions.Children.Add(Caption($"{entry.Author} · {MainWindowPresentation.FormatMessageTime(entry.AtUnixMs)} · изменено файлов: {entry.Changed}"));
            if (entry.Revision is { } released && !string.IsNullOrEmpty(entry.RevisionNote))
                actions.Children.Add(Body($"Ревизия {released}: {entry.RevisionNote}"));
            // What the version holds, before deciding on it.
            Button files = PlainButton($"Файлы в.{entry.Number}", false, () => showFiles(entry.Oid, $"в.{entry.Number} · «{entry.Subject}» · {entry.Author}"));
            files.HorizontalAlignment = HorizontalAlignment.Left;
            actions.Children.Add(files);
            // Нормоконтроль: someone other than who released it approves it — first, above the rest.
            if (entry is { Revision: { } onCheck, RevisionPending: true })
            {
                if (!view.CanReview || entry.RevisionMine)
                    actions.Children.Add(StateLine("На нормоконтроле", "Caution", $"Одобрит {(entry.RevisionMine ? "другой хранитель или админ — не вы" : "хранитель или админ")}."));
                else
                {
                    Button approve = PlainButton($"Одобрить ревизию {onCheck}", true, () => _ = Act(new("approve_revision", id, repo, Oid: entry.Oid),
                        $"Ревизия {onCheck} одобрена — выпущена для всех."));
                    approve.HorizontalAlignment = HorizontalAlignment.Left;
                    actions.Children.Add(approve);
                }
            }
            if (entry.Thinned)
                actions.Children.Add(Body("Содержимое этой версии не сохранено (прорежено правилом хранения) — восстановить её нельзя."));
            else if (entry.Partial)
                actions.Children.Add(Body($"Файлы {MasksText(view.OnlyCurrent)} этой версии не сохранены: новая версия получит остальные файлы в.{entry.Number}."));
            if (view.CanReview && entry.Revision is null)
            {
                actions.Children.Add(SectionLabel("Ревизия"));
                Grid release = new() { ColumnSpacing = 8 };
                release.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(72) });
                release.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
                release.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
                TextBox label = new() { Text = view.NextRevision, PlaceholderText = "A", MaxLength = 16 };
                Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(label, "Обозначение ревизии");
                TextBox note = new() { PlaceholderText = "Комментарий к ревизии", MinWidth = 280 };
                Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(note, "Комментарий к ревизии");
                Grid.SetColumn(note, 1);
                Button go = PlainButton("Выпустить ревизию", false, () => _ = Act(new("release", id, repo, Oid: entry.Oid, Label: label.Text, Note: note.Text),
                    $"в.{entry.Number} выпущена как ревизия {label.Text.Trim()}."));
                Grid.SetColumn(go, 2);
                release.Children.Add(label); release.Children.Add(note); release.Children.Add(go);
                actions.Children.Add(release);
            }
            // Bringing a version back and keeping it stand apart from the decision above.
            StackPanel row = new() { Orientation = Orientation.Horizontal, Spacing = 8, Margin = new Thickness(0, 8, 0, 0) };
            if (!entry.Current && !entry.Thinned)
                row.Children.Add(PlainButton(entry.Partial ? "Создать версию из сохранившихся файлов" : "Восстановить как новую версию", false, () => _ = Act(new("restore", id, repo, Oid: entry.Oid),
                    $"Сохранена новая версия с файлами в.{entry.Number}. Её примут, как любую вашу версию.")));
            if (view.CanReview)
                row.Children.Add(PlainButton(entry.Kept ? "Не хранить навсегда" : "Хранить навсегда", false, () => _ = Act(new("keep", id, repo, Oid: entry.Oid, Keep: !entry.Kept),
                    entry.Kept ? $"в.{entry.Number} больше не хранится навсегда." : $"в.{entry.Number} хранится навсегда.")));
            if (row.Children.Count > 0) actions.Children.Add(row);
        }
        list.SelectionChanged += (_, _) =>
        {
            if ((list.SelectedItem as ListViewItem)?.Tag is WorkspaceHistoryEntry entry && entry.Oid != chosen) { chosen = entry.Oid; DrawChosen(); }
        };
        try
        {
            view = await Task.Run(() => core.WorkspaceFiles(new("history", id, repo)));
            if (live()) Draw();
        }
        catch (Exception ex) { if (live()) SetStatus($"Не удалось прочитать историю: {WorkspaceErrorText.ForCoreMessage(ex.Message)}", StatusKind.Failed); }
    }

    // What everyone keeps of the history, in one sentence.
    private static string RetentionSummary(WorkspaceFilesDetails view)
    {
        int thinned = view.History?.Count(h => h.Thinned) ?? 0;
        return view.RetentionConfirmed
            ? $"Хранится содержимое {RetentionText(view.KeepLast, view.KeepDays)}; ревизий, «навсегда» и текущей — всегда."
              + (view.OnlyCurrent is { Count: > 0 } masks ? $" Файлы {MasksText(masks)} — только в текущей, ревизиях и «навсегда»." : "")
              + " Описание версий остаётся у всех."
              + (thinned > 0 ? $" Прорежено версий: {thinned}." : "")
            : "Хранится вся история: правило хранения ещё не подтверждено админом.";
    }

    // «последних 10 версий и версий за 30 дней»; days 0 count for nothing.
    private static string RetentionText(uint last, uint days) =>
        (last == 1 ? "последней версии" : $"последних {last} версий") + (days > 0 ? $" и версий за {days} дн." : "");

    // «*.stl, *.pdf»
    private static string MasksText(IEnumerable<string>? masks) => string.Join(", ", masks ?? []);

    // A version of the history: number, comment, who and when; chips for a revision, «навсегда», now.
    private FrameworkElement HistoryRow(WorkspaceHistoryEntry entry)
    {
        // The chips under the lines: beside them they squeezed the comment into a column.
        StackPanel row = new() { Spacing = 2, Padding = new Thickness(0, 4, 0, 4) };
        row.Children.Add(new TextBlock { Text = $"в.{entry.Number} · «{entry.Subject}»", TextTrimming = TextTrimming.CharacterEllipsis });
        row.Children.Add(Caption($"{entry.Author} · {MainWindowPresentation.FormatMessageTime(entry.AtUnixMs)} · изменено файлов: {entry.Changed}"));
        StackPanel chips = new() { Orientation = Orientation.Horizontal, Spacing = 6, Margin = new Thickness(0, 2, 0, 0) };
        if (entry.Revision is { } label)
            chips.Children.Add(entry.RevisionPending ? Chip($"Ревизия {label} · на нормоконтроле", "Caution") : Chip($"Ревизия {label}", "Success"));
        if (entry.Kept) chips.Children.Add(Chip("Навсегда", "Accent"));
        if (entry.Thinned) chips.Children.Add(Chip("Прорежена", "Caution"));
        else if (entry.Partial) chips.Children.Add(Chip("Частично", "Caution"));
        if (entry.Current) chips.Children.Add(Chip("Сейчас", "Accent"));
        if (chips.Children.Count > 0) row.Children.Add(chips);
        return row;
    }

    /// «Настройки» of a files project, a tab of its page: first an invitation to keep the project, if
    /// there is one; then «Согласование» — the project's rules; then «Хранение» — how many full copies
    /// there are, who keeps them and how much of the history is kept.
    private async Task FillSettingsAsync(string repo, StackPanel body, Func<bool> live)
    {
        if (bridge is null || groupId is null) return;
        RustBridge core = bridge;
        string id = groupId;
        WorkspaceFilesDetails? view = null;
        bool busy = false;
        async Task Reload()
        {
            await WorkspaceFolderPreferences.EnsureAsync(core);
            view = await Task.Run(() => core.WorkspaceFiles(new("history", id, repo)));
            if (live()) Draw();
        }
        void Draw()
        {
            if (view is null) return;
            body.Children.Clear();
            WorkspaceCustodian? me = view.Members.FirstOrDefault(m => m.IsMe && m.Nominated && m.Nomination is not null);
            if (me is { Nomination: { } nomination })
            {
                body.Children.Add(SectionTitle("Вам предлагают хранить проект", first: true));
                if (!me.StoringHere)
                {
                    body.Children.Add(Body($"Соглашаясь, вы берёте хранение всей истории проекта «{repo}» и право принимать в нём версии участников. Другие проекты workspace это не затрагивает."));
                    body.Children.Add(Body(view.SizeKnown ? $"Нужно места: история {Megabytes(view.HistoryBytes)}, текущие файлы {Megabytes(view.CurrentBytes)}." : "Размер ещё уточняется: дождитесь получения дерева файлов."));
                    TextBox folder = new() { Header = "Папка для хранения истории", Text = view.Folder ?? view.SuggestedFolder ?? view.Root ?? "" };
                    body.Children.Add(folder);
                    StackPanel answer = new() { Spacing = 8 };
                    answer.Children.Add(PlainButton("Выбрать папку…", false, () => _ = ChooseFolder()));
                    async Task ChooseFolder() { if (PickFolder is not null && await PickFolder() is { } path) folder.Text = path; }
                    Button consent = PlainButton("Согласиться и хранить здесь", true,
                        () => _ = Act(new("consent", id, repo, Nomination: nomination, Accepted: true, Folder: folder.Text), "Вы храните проект: история загружается в выбранную папку."));
                    consent.IsEnabled = view.SizeKnown;
                    answer.Children.Add(consent);
                    answer.Children.Add(PlainButton("Отказаться", false,
                        () => _ = Act(new("consent", id, repo, Nomination: nomination, Accepted: false), "Вы отказались хранить проект.")));
                    body.Children.Add(answer);
                }
                else body.Children.Add(PlainButton("Сложить обязанности на этом устройстве", false,
                    () => _ = Act(new("consent", id, repo, Nomination: nomination, Accepted: false), "Обязанности хранителя сложены. Участие и скачанные файлы остались.")));
            }

            // A rule per row, its switch on the right — as the app's settings are.
            body.Children.Add(SectionTitle("Согласование", first: body.Children.Count == 0));
            ToggleSwitch Rule(string header, bool on)
            {
                ToggleSwitch toggle = new() { OnContent = "", OffContent = "", MinWidth = 0, IsOn = on, IsEnabled = view.IAmAdmin };
                Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(toggle, header);
                body.Children.Add(new Shared.SettingRow { Header = header, Control = toggle });
                return toggle;
            }
            ToggleSwitch review = Rule("Правки участников — после принятия", view.Policy.ParticipantsNeedApproval);
            ToggleSwitch download = Rule("Новичкам — вся текущая версия", view.Policy.NewcomersDownloadCurrent);
            ToggleSwitch onlyTaken = Rule("Править файлы — только взяв в работу", view.Policy.EditOnlyTaken);
            ToggleSwitch checkedRevisions = Rule("Ревизия — после нормоконтроля", view.Policy.RevisionsChecked);
            // A switch shows what the project holds: locked while the change goes, back where it was
            // when the core refused, the dialog re-read (its captions too) when it took.
            bool reverting = false;
            async Task PolicyChanged(ToggleSwitch changed)
            {
                if (reverting) return;
                ToggleSwitch[] switches = [review, download, onlyTaken, checkedRevisions];
                foreach (ToggleSwitch s in switches) s.IsEnabled = false;
                bool needsApproval = review.IsOn, downloadCurrent = download.IsOn, strict = onlyTaken.IsOn, check = checkedRevisions.IsOn;
                busy = true;
                try
                {
                    await Task.Run(() => core.WorkspaceFiles(new("policy", id, repo, ParticipantsNeedApproval: needsApproval, NewcomersDownloadCurrent: downloadCurrent, EditOnlyTaken: strict, RevisionsChecked: check)));
                }
                catch (Exception ex)
                {
                    reverting = true;
                    changed.IsOn = !changed.IsOn;
                    reverting = false;
                    foreach (ToggleSwitch s in switches) s.IsEnabled = view?.IAmAdmin == true;
                    SetStatus($"Правило не изменено: {ex.Message}", StatusKind.Failed);
                    return;
                }
                finally { busy = false; }
                try
                {
                    await Reload();
                    SetStatus("Правило сохранено.");
                }
                catch (Exception ex) { SetStatus($"Правило сохранено, но состояние не удалось обновить: {ex.Message}", StatusKind.Warning); }
            }
            review.Toggled += async (_, _) => await PolicyChanged(review);
            download.Toggled += async (_, _) => await PolicyChanged(download);
            onlyTaken.Toggled += async (_, _) => await PolicyChanged(onlyTaken);
            checkedRevisions.Toggled += async (_, _) => await PolicyChanged(checkedRevisions);
            if (busy) foreach (ToggleSwitch s in new[] { review, download, onlyTaken, checkedRevisions }) s.IsEnabled = false;

            body.Children.Add(SectionTitle("Хранение"));
            body.Children.Add(view.SingleCopy
                ? StateLine("Одна копия", "Caution", "Полная история подтверждена только на одном устройстве. Назначьте ещё хранителя.")
                : view.CompleteCopies == 0
                    ? Body("Полнота копий ещё не подтверждена: хранители загружают историю или ещё не сообщили состояние.")
                    : Body($"Полную историю подтвердили устройств: {view.CompleteCopies}."));
            body.Children.Add(Caption(view.SizeKnown ? $"Текущие файлы {Megabytes(view.CurrentBytes)}, содержимое истории {Megabytes(view.HistoryBytes)}. {(view.HistoryMissingBytes > 0 ? $"Недостаёт здесь: {Megabytes(view.HistoryMissingBytes)}." : "Здесь есть всё.")} Рабочая папка занимает дополнительное место." : "Размер ещё уточняется: дождитесь получения дерева файлов."));
            if (view.HistoryPath is { } archive) body.Children.Add(Caption($"История этого проекта: {archive}"));
            foreach (WorkspaceCustodian member in view.Members)
            {
                string role = member.Admin ? "админ" : member.Keeper ? "хранитель" : member.Nominated ? "ждёт согласия" : "участник";
                Shared.SettingRow line = new()
                {
                    Header = $"{member.Name} · {role}",
                    Description = member.Admin || member.Keeper
                        ? member.CopyComplete ? "Полная копия подтверждена" : member.CopyMissingBytes is { } missing ? $"История загружается: недостаёт {Megabytes(missing)}" : "Состояние копии ещё не получено"
                        : "",
                };
                if (view.IAmAdmin && !member.Admin)
                {
                    string action = member.Nominated ? "dismiss" : "nominate";
                    line.Control = PlainButton(member.Nominated ? "Снять назначение" : "Предложить хранить", false,
                        () => _ = Act(new(action, id, repo, AccountId: member.AccountId), member.Nominated ? $"{member.Name} больше не хранитель." : $"{member.Name} получит предложение хранить проект."));
                }
                body.Children.Add(line);
            }

            DrawRetention();
        }
        // «Хранение истории»: what everyone keeps; the admin looks at the report first, then confirms
        // exactly what the report was counted for.
        void DrawRetention()
        {
            if (view is null) return;
            body.Children.Add(SectionTitle("Сколько хранить"));
            body.Children.Add(Body(RetentionSummary(view)));
            if (!view.IAmAdmin) return;
            WorkspaceThinningReport? report = view.Thinning;
            NumberBox last = new() { Header = "Последних версий", Minimum = 1, Maximum = 100000, Value = report?.KeepLast ?? view.KeepLast, SpinButtonPlacementMode = NumberBoxSpinButtonPlacementMode.Compact, Width = 170 };
            NumberBox days = new() { Header = "За дней", Minimum = 0, Maximum = 100000, Value = report?.KeepDays ?? view.KeepDays, SpinButtonPlacementMode = NumberBoxSpinButtonPlacementMode.Compact, Width = 170 };
            (uint, uint) Rule() => ((uint)Math.Clamp(double.IsNaN(last.Value) ? 1 : last.Value, 1, 100000), (uint)Math.Clamp(double.IsNaN(days.Value) ? 0 : days.Value, 0, 100000));
            StackPanel inputs = new() { Orientation = Orientation.Horizontal, Spacing = 8 };
            inputs.Children.Add(last); inputs.Children.Add(days);
            body.Children.Add(inputs);
            TextBox onlyCurrent = new()
            {
                Header = "Только в текущей версии",
                PlaceholderText = "*.stl, *.step, *.pdf",
                Text = string.Join(", ", report?.OnlyCurrent ?? view.OnlyCurrent ?? []),
            };
            body.Children.Add(onlyCurrent);
            List<string> Masks() => onlyCurrent.Text.Split([',', ';'], StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries).ToList();
            StackPanel buttons = new() { Orientation = Orientation.Horizontal, Spacing = 8 };
            Button Count() => PlainButton("Что будет удалено", true, () =>
            {
                (uint n, uint d) = Rule();
                _ = Kept(new("thinning", id, repo, KeepLast: n, KeepDays: d, OnlyCurrent: Masks()), "");
            });
            if (report is null) buttons.Children.Add(Count());
            else
            {
                // The report is for the numbers it was counted with: a field changed — it and its
                // «Подтвердить» go, and «Что будет удалено» counts again.
                StackPanel counted = new() { Spacing = 2 };
                void Stale()
                {
                    if (counted.Visibility == Visibility.Collapsed) return;
                    counted.Visibility = Visibility.Collapsed;
                    buttons.Children.Clear();
                    buttons.Children.Add(Count());
                }
                last.ValueChanged += (_, _) => Stale();
                days.ValueChanged += (_, _) => Stale();
                onlyCurrent.TextChanged += (_, _) => Stale();
                counted.Children.Add(Body(report.Versions.Count == 0
                    ? "По этому правилу ни одна версия не потеряет содержимое."
                    : $"Потеряют содержимое ({report.Versions.Count}), освободится {Megabytes(report.FreedBytes)}:"));
                foreach (WorkspaceHistoryEntry entry in report.Versions.Take(8))
                    counted.Children.Add(Caption($"  в.{entry.Number} · «{entry.Subject}» · {MainWindowPresentation.FormatMessageTime(entry.AtUnixMs)}"));
                if (report.Versions.Count > 8) counted.Children.Add(Caption($"  и ещё {report.Versions.Count - 8}"));
                if (report.PartialVersions > 0)
                    counted.Children.Add(Body($"Без файлов {MasksText(report.OnlyCurrent)} останутся версий: {report.PartialVersions}."));
                if (report.RestoredVersions > 0)
                    counted.Children.Add(Body($"Снова под хранением: {report.RestoredVersions} (содержимое вернётся от тех, у кого оно ещё есть)."));
                counted.Children.Add(Body("После подтверждения каждый участник удаляет это у себя сам, и дальше правило работает само."));
                body.Children.Add(counted);
                buttons.Children.Add(PlainButton("Подтвердить правило", true, () =>
                    _ = Kept(new("retain", id, repo, KeepLast: report.KeepLast, KeepDays: report.KeepDays, OnlyCurrent: report.OnlyCurrent),
                        $"Правило подтверждено: хранится содержимое {RetentionText(report.KeepLast, report.KeepDays)}.")));
            }
            body.Children.Add(buttons);
        }
        // One command at a time: a second press while one is under way is not sent.
        async Task Act(WorkspaceFilesRequest request, string done)
        {
            if (busy) return;
            busy = true;
            try { await Task.Run(() => core.WorkspaceFiles(request)); }
            catch (Exception ex) { SetStatus(ex.Message, StatusKind.Failed); return; }
            finally { busy = false; }
            try { await Reload(); SetStatus(done); }
            catch (Exception ex) { SetStatus($"{done} Состояние не удалось обновить: {ex.Message}", StatusKind.Warning); }
        }
        // The retention commands answer with the history and the report themselves: drawn as they come.
        async Task Kept(WorkspaceFilesRequest request, string done)
        {
            if (busy) return;
            busy = true;
            try { view = await Task.Run(() => core.WorkspaceFiles(request)); Draw(); SetStatus(done); }
            catch (Exception ex) { SetStatus(ex.Message, StatusKind.Failed); }
            finally { busy = false; }
        }
        try { await Reload(); }
        catch (Exception ex) { if (live()) SetStatus($"Не удалось прочитать настройки: {WorkspaceErrorText.ForCoreMessage(ex.Message)}", StatusKind.Failed); }
    }

    private static readonly string[] PictureTypes = [".png", ".jpg", ".jpeg", ".bmp", ".gif", ".tif", ".tiff", ".ico", ".webp"];

    // Pictures and PDF are shown by Windows itself; models open in their program (previews of CAD
    // files — later, docs/decisions/workspace-files-storage.md).
    private static bool Previewable(string path)
    {
        string type = Path.GetExtension(path).ToLowerInvariant();
        return type == ".pdf" || PictureTypes.Contains(type);
    }

    // The file as a picture: an image as it is, a PDF's first page. The bytes are read into memory so
    // the file (in the working folder, maybe) is not held open.
    private static async Task<FrameworkElement> RenderPreviewAsync(string file, string path, double height = 280)
    {
        try
        {
            byte[] bytes = await File.ReadAllBytesAsync(file);
            using global::Windows.Storage.Streams.InMemoryRandomAccessStream source = new();
            await source.WriteAsync(System.Runtime.InteropServices.WindowsRuntime.WindowsRuntimeBufferExtensions.AsBuffer(bytes));
            source.Seek(0);
            Microsoft.UI.Xaml.Media.Imaging.BitmapImage picture = new();
            string caption = "";
            if (Path.GetExtension(path).Equals(".pdf", StringComparison.OrdinalIgnoreCase))
            {
                global::Windows.Data.Pdf.PdfDocument document = await global::Windows.Data.Pdf.PdfDocument.LoadFromStreamAsync(source);
                using global::Windows.Data.Pdf.PdfPage page = document.GetPage(0);
                using global::Windows.Storage.Streams.InMemoryRandomAccessStream rendered = new();
                await page.RenderToStreamAsync(rendered, new global::Windows.Data.Pdf.PdfPageRenderOptions { DestinationWidth = 600 });
                rendered.Seek(0);
                await picture.SetSourceAsync(rendered);
                caption = document.PageCount > 1 ? $"стр. 1 из {document.PageCount}" : "";
            }
            else await picture.SetSourceAsync(source);
            StackPanel shown = new() { Spacing = 2 };
            // A thin frame: a white page must not melt into a light dialog.
            shown.Children.Add(new Border
            {
                BorderThickness = new Thickness(1),
                BorderBrush = MainWindowPresentation.ResolveThemeBrush("OmniPopupStrokeBrush", new Microsoft.UI.Xaml.Media.SolidColorBrush(Microsoft.UI.Colors.Gray)),
                HorizontalAlignment = HorizontalAlignment.Left,
                Child = new Image { Source = picture, Stretch = Microsoft.UI.Xaml.Media.Stretch.Uniform, MaxHeight = height },
            });
            if (caption.Length > 0) shown.Children.Add(Caption(caption));
            return shown;
        }
        catch (Exception) { return Caption("Не удалось показать файл: формат не читается."); }
    }
}
