import ShellswimKit
import SwiftUI

struct ChatView: View {
    @ObservedObject var model: ChatModel
    @Environment(\.scenePhase) private var phase
    @State private var showSettings = false
    private let timer = Timer.publish(every: 0.25, on: .main, in: .common).autoconnect()

    var body: some View {
        NavigationStack {
            VStack(spacing: 0) {
                ScrollViewReader { proxy in
                    ScrollView {
                        LazyVStack(spacing: 6) {
                            ForEach(model.items) { BubbleView(item: $0) }
                        }
                        .padding(.horizontal, 12)
                    }
                    .onChange(of: model.items.last?.id) { id in
                        if let id { proxy.scrollTo(id, anchor: .bottom) }
                    }
                }
                Divider()
                HStack {
                    TextField("command", text: $model.input)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .textFieldStyle(.roundedBorder)
                        .onSubmit { model.send() }
                    Button("Send") { model.send() }
                        .disabled(model.input.trimmingCharacters(in: .whitespaces).isEmpty)
                }
                .padding(8)
            }
            .navigationTitle("shellswim")
            .toolbar {
                Button { showSettings = true } label: { Image(systemName: "gearshape") }
            }
            .sheet(isPresented: $showSettings, onDismiss: { model.connect() }) {
                SettingsView()
            }
        }
        .onReceive(timer) { _ in if phase == .active { model.poll() } }
        .onAppear { model.connect() }
    }
}

struct BubbleView: View {
    let item: ChatItem

    var body: some View {
        HStack {
            if item.role == .user { Spacer(minLength: 40) }
            Text(item.text)
                .font(.system(.body, design: .monospaced))
                .foregroundStyle(item.role == .user ? Color.white : item.role == .error ? Color.red : Color.primary)
                .italic(item.role == .system)
                .padding(8)
                .background(background, in: RoundedRectangle(cornerRadius: 12))
                .textSelection(.enabled)
            if item.role != .user { Spacer(minLength: 40) }
        }
    }

    private var background: Color {
        switch item.role {
        case .user: return .blue
        case .reply: return Color(.secondarySystemBackground)
        case .system: return Color(.tertiarySystemBackground)
        case .error: return Color.red.opacity(0.15)
        }
    }
}
