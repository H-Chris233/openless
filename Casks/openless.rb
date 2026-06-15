cask "openless" do
  arch arm: "aarch64", intel: "x64"

  version "1.3.9"
  sha256 arm:   "593af5f495305dec67893cb8dfde5f8edec832b0889735b5b6ec06bc3f28991e",
         intel: "877d420ee5cba48e9c153535f0d65f8460a08ee00cd9f63cb8452412a1901bbd"

  url "https://github.com/appergb/openless/releases/download/v#{version}-tauri/OpenLess_#{version}_#{arch}.dmg"
  name "OpenLess"
  desc "Menu-bar voice input layer for macOS"
  homepage "https://github.com/appergb/openless"

  livecheck do
    url :url
    regex(/^v?(\d+(?:\.\d+)+)[._-]tauri$/i)
  end

  auto_updates true

  app "OpenLess.app"

  zap trash: [
    "~/Library/Application Support/OpenLess",
    "~/Library/Caches/com.openless.app",
    "~/Library/Logs/OpenLess",
    "~/Library/Preferences/com.openless.app.plist",
    "~/Library/WebKit/com.openless.app",
  ]
end
