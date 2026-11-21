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
                GlassGroup(spacing: 8) {
                    HStack(spacing: 8) {
                        TextField("command", text: $model.input)
                            .textInputAutocapitalization(.never)
                            .autocorrectionDisabled()
                            .onSubmit { model.send() }
                            .padding(.horizontal, 16)
                            .padding(.vertical, 10)
                            .glassSurface(Capsule())
                        Button { model.send() } label: {
                            Image(systemName: "arrow.up").font(.body.weight(.semibold))
                        }
                        .glassCircleButton()
                        .accessibilityLabel("Send")
                        .disabled(model.input.trimmingCharacters(in: .whitespaces).isEmpty)
                    }
                }
                .padding(.horizontal, 12)
                .padding(.vertical, 8)
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
                .foregroundStyle(Color.primary)
                .italic(item.role == .system)
                .padding(.horizontal, 12)
                .padding(.vertical, 8)
                .glassSurface(RoundedRectangle(cornerRadius: 18, style: .continuous), tint: tint)
                .textSelection(.enabled)
            if item.role != .user { Spacer(minLength: 40) }
        }
    }

    /// Light tints keep the primary-colour text readable on glass.
    private var tint: Color? {
        switch item.role {
        case .user: return Color.blue.opacity(0.45)
        case .reply, .system: return nil
        case .error: return Color.red.opacity(0.3)
        }
    }
}
