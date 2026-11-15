using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Data;
using Microsoft.UI.Xaml.Input;

namespace Omni.Windows.App.Features.Shared;

/// One line of the panel; the last of a group may be short.
public sealed class EmojiRow(string[] cells)
{
    public string C0 => At(0);
    public string C1 => At(1);
    public string C2 => At(2);
    public string C3 => At(3);
    public string C4 => At(4);
    public string C5 => At(5);
    public string C6 => At(6);
    public string C7 => At(7);

    internal string At(int cell) => cell >= 0 && cell < cells.Length ? cells[cell] : string.Empty;
}

/// One group of the panel: its rows of emoji, and the title over them.
public sealed class EmojiGroup(string title, string emoji) : List<EmojiRow>(
    emoji.Split(' ', StringSplitOptions.RemoveEmptyEntries).Chunk(EmojiPanel.CellsInRow).Select(cells => new EmojiRow(cells)))
{
    public string Title { get; } = title;
}

/// See EmojiPanel.xaml. Built when first opened: most conversations never open it.
public sealed partial class EmojiPanel : UserControl
{
    internal const int CellsInRow = 8;
    private const double CellSize = 38;

    private static readonly EmojiGroup[] Groups =
    [
        new("Смайлы", "😀 😃 😄 😁 😆 😅 😂 🤣 🥲 🙂 🙃 😉 😊 😇 🥰 😍 🤩 😘 😗 😚 😙 😋 😛 😜 🤪 😝 🤑 🤗 🤭 🤫 🤔 🫡 🤐 🤨 😐 😑 😶 😏 😒 🙄 😬 😮‍💨 🤥 😌 😔 😪 🤤 😴 😷 🤒 🤕 🤢 🤮 🤧 🥵 🥶 🥴 😵 🤯 🤠 🥳 🥸 😎 🤓 🧐 😕 😟 🙁 😮 😯 😲 😳 🥺 😦 😧 😨 😰 😥 😢 😭 😱 😖 😣 😞 😓 😩 😫 🥱 😤 😡 😠 🤬 😈 👿 💀 ☠️ 💩 🤡 👻 👽 🤖"),
        new("Жесты и люди", "👋 🤚 🖐️ ✋ 🖖 👌 🤌 🤏 ✌️ 🤞 🤟 🤘 🤙 👈 👉 👆 👇 ☝️ 👍 👎 ✊ 👊 🤛 🤜 👏 🙌 👐 🤲 🤝 🙏 ✍️ 💅 🤳 💪 🧠 👀 👁️ 👅 👄 🙋 🙅 🙆 🤷 🤦 🧑‍💻 🧑‍🔧 🧑‍🚀 🚶 🏃 💃 🕺"),
        new("Сердца и знаки", "❤️ 🧡 💛 💚 💙 💜 🖤 🤍 🤎 💔 ❤️‍🔥 💕 💞 💓 💗 💖 💘 💝 💯 💢 💥 💫 💦 💨 🔥 ✨ ⭐ 🌟 ⚡ ✅ ❌ ❓ ❗ ⚠️ 🚫 ♻️ 🔔 🔕 💤 💬 👁️‍🗨️ 🎉 🎊 🎈 🎁 🏆 🥇 🥈 🥉"),
        new("Животные и природа", "🐶 🐱 🐭 🐹 🐰 🦊 🐻 🐼 🐨 🐯 🦁 🐮 🐷 🐸 🐵 🙈 🙉 🙊 🐔 🐧 🐦 🦆 🦅 🦉 🐺 🐗 🐴 🦄 🐝 🐛 🦋 🐌 🐞 🐜 🕷️ 🐢 🐍 🦎 🐙 🐬 🐳 🦈 🌵 🌲 🌳 🌴 🌱 🍀 🍁 🌸 🌹 🌻 🌞 🌙 ☀️ ⛅ 🌧️ ❄️ ☃️ 🌈 🌊"),
        new("Еда", "🍏 🍎 🍐 🍊 🍋 🍌 🍉 🍇 🍓 🍒 🍑 🥭 🍍 🥥 🥝 🍅 🥑 🥦 🥒 🌶️ 🌽 🥕 🥔 🥐 🍞 🧀 🥚 🍳 🥓 🥩 🍗 🍔 🍟 🍕 🌭 🥪 🌮 🍝 🍜 🍣 🍰 🎂 🍩 🍪 🍫 🍬 🍯 ☕ 🍵 🍺 🍻 🥂 🍷 🥃 🍾"),
        new("Дела и вещи", "⚽ 🏀 🏐 🎾 🏓 🥊 🎯 🎮 🎲 🧩 🎵 🎶 🎧 🎤 🎬 📷 📹 📱 💻 🖥️ ⌨️ 🖱️ 💾 📁 📄 📊 📈 📉 📌 📎 ✂️ 🔒 🔓 🔑 🔧 🔨 ⚙️ 🧰 🧲 💡 🔋 🔌 💰 💳 📦 ✉️ 📅 ⏰ ⌛ 🚀 ✈️ 🚗 🚲 🏠 🏢 🗺️ 🐛"),
    ];

    public EmojiPanel()
    {
        InitializeComponent();
        ListPart.ItemsSource = new CollectionViewSource { IsSourceGrouped = true, Source = Groups }.View;
    }

    /// An emoji pressed.
    internal event EventHandler<string>? Chosen;

    private static string EmojiAt(Grid row, double x, out int cell)
    {
        cell = (int)(x / CellSize);
        return row.DataContext is EmojiRow emoji ? emoji.At(cell) : string.Empty;
    }

    private void OnRowTapped(object sender, TappedRoutedEventArgs e)
    {
        var row = (Grid)sender;
        string emoji = EmojiAt(row, e.GetPosition(row).X, out _);
        if (emoji.Length > 0)
        {
            Chosen?.Invoke(this, emoji);
        }
    }

    // One highlight per row, moved to the cell under the pointer. It is the row's first child.
    private void OnRowMoved(object sender, PointerRoutedEventArgs e)
    {
        var row = (Grid)sender;
        var highlight = (FrameworkElement)row.Children[0];
        bool over = EmojiAt(row, e.GetCurrentPoint(row).Position.X, out int cell).Length > 0;
        highlight.Visibility = over ? Visibility.Visible : Visibility.Collapsed;
        if (over)
        {
            Grid.SetColumn(highlight, cell);
        }
    }

    private void OnRowExited(object sender, PointerRoutedEventArgs e) =>
        ((Grid)sender).Children[0].Visibility = Visibility.Collapsed;
}
