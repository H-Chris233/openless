cask "openless" do
  arch arm: "aarch64", intel: "x64"

  version "1.3.17"
  sha256 arm:   "405467056c64c05fe13771e00c316dd3b4d344333fe413b45b6bc6e828b6e742",
         intel: "3079e07102ef3842a51f78f5d3427cfce1648db33b436e61b68183060d1f2a62"

  url "https://github.com/Open-Less/openless/releases/download/v#{version}-tauri/OpenLess_#{version}_#{arch}.dmg"
  name "OpenLess"
  desc "Menu-bar voice input layer"
  homepage "https://github.com/Open-Less/openless"

  livecheck do
    url :url
    regex(/^v?(\d+(?:\.\d+)+)[._-]tauri$/i)
  end

  auto_updates true
  depends_on macos: :monterey

  app "OpenLess.app"

  zap trash: [
    "~/Library/Application Support/OpenLess",
    "~/Library/Caches/com.openless.app",
    "~/Library/Logs/OpenLess",
    "~/Library/Preferences/com.openless.app.plist",
    "~/Library/WebKit/com.openless.app",
  ]
end
