using System.Text.Json;
using Omni.Windows.App.Core;
using Xunit;

namespace Omni.Windows.App.Tests;

public sealed class WorkspaceFilesContractTests
{
    [Fact]
    public void ReviewCommand_UsesTheExactVersionAndExplicitWireNames()
    {
        WorkspaceFilesRequest request = new("review", "group", "models", Accepted: true, Oid: new string('a', 40));
        string json = JsonSerializer.Serialize(request, OmniJsonSerializerContext.Default.WorkspaceFilesRequest);
        using JsonDocument document = JsonDocument.Parse(json);
        Assert.Equal("review", document.RootElement.GetProperty("action").GetString());
        Assert.Equal("group", document.RootElement.GetProperty("group_id").GetString());
        Assert.Equal(new string('a', 40), document.RootElement.GetProperty("oid").GetString());
        Assert.True(document.RootElement.GetProperty("accepted").GetBoolean());
        Assert.False(document.RootElement.TryGetProperty("sender", out _));
    }

    [Fact]
    public void FileTree_PreservesMetadataWithoutClaimingContentIsPresent()
    {
        const string json = """
            {"policy":{"participants_need_approval":true,"newcomers_download_current":false,"changed_at_unix_ms":1},
             "i_am_admin":false,"can_review":false,"members":[],
             "files":[{"path":"models/part.prt","bytes":1000000,"available_here":false}],
             "proposals":[],"work_path":null,"root":null,"suggested_folder":null,"folder":null,
             "history_bytes":5000000,"history_missing_bytes":5000000,"current_bytes":1000000,"single_copy":true,"download_status":null}
            """;
        WorkspaceFilesDetails view = JsonSerializer.Deserialize(json, OmniJsonSerializerContext.Default.WorkspaceFilesDetails)!;
        Assert.True(view.Policy.ParticipantsNeedApproval);
        Assert.False(view.Policy.NewcomersDownloadCurrent);
        Assert.Null(view.WorkPath);
        WorkspaceFile file = Assert.Single(view.Files);
        Assert.Equal("models/part.prt", file.Path);
        Assert.Equal(1000000, file.Bytes);
        Assert.False(file.AvailableHere);
    }
}
