import SwiftUI

@main
struct ShellswimApp: App {
    @StateObject private var model = ChatModel()

    var body: some Scene {
        WindowGroup { ChatView(model: model) }
    }
}
