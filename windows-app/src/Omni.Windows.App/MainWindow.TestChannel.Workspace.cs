using Omni.Windows.App.Core;

namespace Omni.Windows.App;

/// <summary>
/// Workspace commands for the UI test channel (docs/decisions/ui-test-channel.md): the live pass of
/// a group with git repositories between machines (docs/decisions/omni-workspace-git.md). Same
/// bridge calls a workspace screen will make; the announce and fetch run in the core's mesh tick.
/// </summary>
public sealed partial class MainWindow
{
    private async Task<string?> TryExecuteWorkspaceUiTestCommandAsync(string command, IReadOnlyList<string> args)
    {
        string Arg(int i) => i < args.Count ? args[i] : string.Empty;
        switch (command)
        {
            // Same file-workspace bridge and dialog handlers as the UI. JSON is base64 to preserve
            // arbitrary file names and local paths through the test channel's word parser.
            case "ws.files-json":
            {
                string json = System.Text.Encoding.UTF8.GetString(Convert.FromBase64String(Arg(0)));
                WorkspaceFilesRequest request = System.Text.Json.JsonSerializer.Deserialize(json, OmniJsonSerializerContext.Default.WorkspaceFilesRequest)
                    ?? throw new InvalidOperationException("empty workspace files request");
                WorkspaceFilesDetails result = await Task.Run(() => rustBridge.WorkspaceFiles(request));
                return "ok " + System.Text.Json.JsonSerializer.Serialize(result, OmniJsonSerializerContext.Default.WorkspaceFilesDetails);
            }
            case "ws.files-ui":
                ShowWorkspace(Arg(0));
                // [path [в.N]]: the file's preview, beside version N.
                _ = WorkspacesSectionHost.ShowFilesAsync(Arg(1), Arg(2).Length > 0 ? Arg(2) : null, int.TryParse(Arg(3), out int compare) ? compare : 0);
                return "ok opening files";
            case "ws.history-ui":
                ShowWorkspace(Arg(0));
                _ = WorkspacesSectionHost.ShowHistoryAsync(Arg(1));
                return "ok opening history";
            // ws.code <group_id> <repo> — a code project's page (its row in the list pressed).
            case "ws.code":
                ShowWorkspace(Arg(0));
                WorkspacesSectionHost.OpenCode(Arg(1));
                return "ok";
            case "ws.custody-ui":
                ShowWorkspace(Arg(0));
                _ = WorkspacesSectionHost.ShowCustodyAsync(Arg(1));
                return "ok opening custody";
            // ws.add-repo <group_id> <repo> [<working repository path>]
            case "ws.add-repo":
            {
                string? work = args.Count > 2 ? string.Join(' ', args.Skip(2)) : null;
                WorkspaceRepoView added = rustBridge.AddWorkspaceRepo(Arg(0), Arg(1), work);
                return $"ok published={added.PublishedPath}";
            }

            case "ws.repos":
                return "ok" + string.Concat(rustBridge.ListWorkspaceRepos(Arg(0))
                    .Select(r => $"{Environment.NewLine}repo\t{r.Repo}\tpublished={r.PublishedPath}\twork={r.WorkPath ?? "-"}"));

            // ws.branches <group_id> <repo> — account, branch, commit, here|wanted
            case "ws.branches":
                return "ok" + string.Concat(rustBridge.ListWorkspaceBranches(Arg(0), Arg(1))
                    .Select(b => $"{Environment.NewLine}branch\t{b.AccountId}\t{b.RefName}\t{b.Oid}\t{(b.Fetched ? "here" : "wanted")}"));

            case "ws.main":
            {
                WorkspaceMainView main = rustBridge.GetWorkspaceMain(Arg(0), Arg(1));
                static string Pairs(List<List<string>> list) => string.Join(",", list.Select(p => string.Join(":", p)));
                return $"ok main={main.Oid ?? "-"} rule={(main.NeedsApproval ? "approval" : "any")} waiting={Pairs(main.Waiting)} forks={Pairs(main.Forks)}";
            }

            // ws.rule <group_id> <repo> any|approval
            case "ws.rule":
                if (Arg(2) is not ("any" or "approval"))
                {
                    return "error: ws.rule needs a group id, a repo and any|approval";
                }
                rustBridge.SetWorkspaceMainRule(Arg(0), Arg(1), Arg(2) == "approval");
                return "ok";

            // ws.approve <group_id> <repo> <commit>
            case "ws.approve":
                rustBridge.ApproveWorkspaceCommit(Arg(0), Arg(1), Arg(2));
                return "ok";

            // The Workspaces section (docs/decisions/omni-workspace-ux.md): its buttons by their product
            // handlers — a folder chosen in the dialog or dropped comes as an argument.
            // ws.open <group_id> — the section on that workspace (as «Код» on a group's page)
            case "ws.open":
                ShowWorkspace(Arg(0));
                return "ok" + Environment.NewLine + await WorkspacesSectionHost.DescribeForUiTestAsync();

            // ws.unbind <group_id> <repo> — «Убрать папку…» (confirmed)
            case "ws.unbind":
            {
                ShowWorkspace(Arg(0));
                WorkspaceRepoState? repo = rustBridge.GetWorkspaceScreen(Arg(0)).Repos.FirstOrDefault(r => r.Repo == Arg(1));
                if (repo is null) return "error: no such project";
                await WorkspacesSectionHost.UnbindFolderAsync(repo, ask: false);
                return "ok" + Environment.NewLine + await WorkspacesSectionHost.DescribeForUiTestAsync();
            }

            // ws.tasks <group_id> — the board «Задачи» (task commands: ws.files-json with "action":"task_…")
            case "ws.tasks":
                ShowWorkspace(Arg(0));
                await WorkspacesSectionHost.ShowTasksAsync();
                return "ok";

            // ws.agents <group_id> — the agents' place of the switcher.
            case "ws.agents":
                ShowWorkspace(Arg(0));
                await WorkspacesSectionHost.ShowAgentsAsync();
                return "ok" + Environment.NewLine + await WorkspacesSectionHost.DescribeForUiTestAsync();

            // ws.talk <group_id> [thread] — «Обсуждения», the latest or the one named.
            case "ws.talk":
                ShowWorkspace(Arg(0));
                await WorkspacesSectionHost.ShowTalkAsync(Arg(1).Length > 0 ? Arg(1) : null);
                return "ok" + Environment.NewLine + await WorkspacesSectionHost.DescribeForUiTestAsync();

            // ws.agents-json <base64 UTF-8 JSON> — the agents' command as the page sends it.
            case "ws.agents-json":
            {
                string json = System.Text.Encoding.UTF8.GetString(Convert.FromBase64String(Arg(0)));
                WorkspaceAgentsRequest request = System.Text.Json.JsonSerializer.Deserialize(json, OmniJsonSerializerContext.Default.WorkspaceAgentsRequest)
                    ?? throw new InvalidOperationException("empty workspace agents request");
                WorkspaceAgents result = await Task.Run(() => rustBridge.WorkspaceAgents(request));
                return "ok " + System.Text.Json.JsonSerializer.Serialize(result, OmniJsonSerializerContext.Default.WorkspaceAgents);
            }

            // ws.back <group_id> <repo> — «Вернуть к общей…» (confirmed)
            case "ws.back":
            {
                ShowWorkspace(Arg(0));
                WorkspaceRepoState? repo = rustBridge.GetWorkspaceScreen(Arg(0)).Repos.FirstOrDefault(r => r.Repo == Arg(1));
                if (repo is null) return "error: no such project";
                await WorkspacesSectionHost.BackToSharedAsync(repo, ask: false);
                return "ok" + Environment.NewLine + await WorkspacesSectionHost.DescribeForUiTestAsync();
            }

            // ws.show <repo> — scroll the page to the project's card (for ui.shot)
            case "ws.show":
                return WorkspacesSectionHost.BringRepoIntoView(Arg(0)) ? "ok" : "error: no card for " + Arg(0);

            // ws.cards — «К проектам» on a project's page
            case "ws.cards":
                WorkspacesSectionHost.BackToCards();
                return "ok";

            case "ws.page":
                return "ok" + Environment.NewLine + await WorkspacesSectionHost.DescribeForUiTestAsync();

            // ws.new [--files] <folder> — «+ Workspace» / a folder dropped on the list (the answer to «Что в
            // папке?»: --files — «Файлы», otherwise «Код»)
            case "ws.new":
            {
                AuthenticatedNavigationView.SelectedItem = WorkspacesNavigationItem;
                bool files = Arg(0) == "--files";
                await WorkspacesSectionHost.NewWorkspaceAsync(string.Join(' ', args.Skip(files ? 1 : 0)), files);
                return "ok" + Environment.NewLine + await WorkspacesSectionHost.DescribeForUiTestAsync();
            }

            // ws.bind <group_id> [--files] <folder> — «Добавить папку» / a folder dropped on the page
            case "ws.bind":
            {
                ShowWorkspace(Arg(0));
                bool files = Arg(1) == "--files";
                await WorkspacesSectionHost.BindFolderAsync(string.Join(' ', args.Skip(files ? 2 : 1)), files);
                return "ok" + Environment.NewLine + await WorkspacesSectionHost.DescribeForUiTestAsync();
            }

            // ws.save <group_id> <repo> [comment…] — «Сохранить версию» of a files folder
            case "ws.save":
                ShowWorkspace(Arg(0));
                await WorkspacesSectionHost.SaveVersionAsync(Arg(1), string.Join(' ', args.Skip(2)));
                return "ok" + Environment.NewLine + await WorkspacesSectionHost.DescribeForUiTestAsync();

            // ws.take <group_id> <repo> <folder> — «Забрать сюда»
            case "ws.take":
                ShowWorkspace(Arg(0));
                await WorkspacesSectionHost.TakeRepoAsync(Arg(1), string.Join(' ', args.Skip(2)));
                return "ok" + Environment.NewLine + await WorkspacesSectionHost.DescribeForUiTestAsync();

            // ws.pull <group_id> <repo> — «Забрать»
            case "ws.pull":
                ShowWorkspace(Arg(0));
                await WorkspacesSectionHost.PullAsync(Arg(1));
                return "ok" + Environment.NewLine + await WorkspacesSectionHost.DescribeForUiTestAsync();

            // ws.mine <group_id> <repo> <branch> — «На мои устройства»
            case "ws.mine":
                rustBridge.SendWorkspaceBranchToMyDevices(Arg(0), Arg(1), Arg(2));
                return "ok";

            // ws.take-device <group_id> <repo> <branch> — «Забрать» a branch from our other device
            case "ws.take-device":
            {
                ShowWorkspace(Arg(0));
                WorkspaceDeviceBranch? theirs = rustBridge.GetWorkspaceScreen(Arg(0)).Repos
                    .FirstOrDefault(r => r.Repo == Arg(1))?.DeviceBranches?.FirstOrDefault(b => b.Branch == Arg(2));
                if (theirs is null)
                {
                    return "error: no such branch from our devices";
                }
                await WorkspacesSectionHost.TakeFromDeviceAsync(Arg(1), theirs);
                return "ok" + Environment.NewLine + await WorkspacesSectionHost.DescribeForUiTestAsync();
            }

            // ws.publish <group_id> <repo> <branch> — «Опубликовать ветку»
            case "ws.publish":
                rustBridge.PublishWorkspaceBranch(Arg(0), Arg(1), Arg(2));
                return "ok";

            default:
                return null;
        }
    }
}
