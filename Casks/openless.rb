cask "openless" do
  arch arm: "aarch64", intel: "x64"

  version "1.3.7"
  sha256 arm:   "ed2fb935741dc5efa5c2ebb86f5e04c0ac2fbffa27166d92d211f103327024b4",
         intel: "b271a941b7cd2df1655104960e032e2f3d4c51d1ebe6d34cc58a90bbaa0e1193"

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
