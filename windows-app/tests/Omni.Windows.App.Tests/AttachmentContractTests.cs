using System.Text.Json;
using Omni.Windows.App.Core;
using Xunit;

namespace Omni.Windows.App.Tests;

public sealed class AttachmentContractTests
{
    /// V-18: the core flags a file that opens as a program; the shell reads the flag and asks first.
    [Fact]
    public void AttachmentRecord_ReadsTheCoresProgramFlag()
    {
        const string program = """
            {"message_id":"m","file_name":"invoicefdp.exe","file_size_bytes":10,"content_id_hex":"ab",
             "state":"downloaded","bytes_transferred":10,"local_blob_available":true,"local_serving_enabled":false,
             "remote_holder_available":false,"opens_as_program":true}
            """;
        Assert.True(JsonSerializer.Deserialize<AttachmentRecord>(program)!.OpensAsProgram);

        // The core leaves the field out for a document.
        const string document = """
            {"message_id":"m","file_name":"report.pdf","file_size_bytes":10,"content_id_hex":"ab",
             "state":"downloaded","bytes_transferred":10,"local_blob_available":true,"local_serving_enabled":false,
             "remote_holder_available":false}
            """;
        Assert.False(JsonSerializer.Deserialize<AttachmentRecord>(document)!.OpensAsProgram);
    }
}
