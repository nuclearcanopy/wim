import ShellswimKit
import SwiftUI

struct SettingsView: View {
    @Environment(\.dismiss) private var dismiss
    @State private var settings = KeychainStore().load()
    @State private var message: String?

    var body: some View {
        NavigationStack {
            Form {
                Section("Navidrome (https only)") {
                    TextField("https://music.example.com", text: $settings.url)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .keyboardType(.URL)
                    TextField("user", text: $settings.user)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                    SecureField("password", text: $settings.password)
                }
                if let message { Text(message).foregroundStyle(.red) }
            }
            .navigationTitle("Settings")
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
                ToolbarItem(placement: .confirmationAction) { Button("Save", action: save) }
            }
        }
    }

    private func save() {
        settings.url = settings.url.trimmingCharacters(in: .whitespaces)
        guard ServerSettings.isValidServerURL(settings.url) else {
            message = "The server URL must start with https://"
            return
        }
        do {
            try KeychainStore().save(settings)
            dismiss()
        } catch {
            message = "\(error)"
        }
    }
}
