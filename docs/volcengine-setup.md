# OpenLess 火山 ASR 配置

1. 登录火山引擎  
   <https://console.volcengine.com/auth/login/>

   ![登录火山引擎](./images/volcengine-setup/01-login.png)

2. 创建旧版应用  
   创建时勾选 `豆包流式语音识别模型2.0 小时版`  
   <https://console.volcengine.com/speech/app?opt=create>

   ![创建旧版应用并勾选小时版](./images/volcengine-setup/02-create-legacy-app.png)

3. 打开豆包流式语音识别模型 2.0 管理页  
   `APP ID` 和 `Access Token` 在页面最下方  
   <https://console.volcengine.com/speech/service/10038?AppID=&opt=create>

   ![流式语音识别模型 2.0 管理页](./images/volcengine-setup/03-streaming-asr-page.png)

4. 复制到 OpenLess 的 `Settings` 页面

   打开：

   `Settings -> Providers -> ASR`

   ![复制到 OpenLess 的 Settings 页面](./images/volcengine-setup/04-openless-settings.png)

   填这两个：

   - `APP ID`
   - `Access Token`

   不用填：

   - `Secret Key`

## 新版控制台（API Key 方式）

新版豆包语音控制台统一使用单个 `API Key` 鉴权，无需 `APP ID` / `Access Token`（旧版应用方式见上文）。

1. 在新版语音控制台创建 API Key  
   <https://console.volcengine.com/speech/new/setting/apikeys>

2. 在 OpenLess 的 `Settings -> Providers -> ASR` 中：

   - 鉴权模式选择「新版控制台 API Key」
   - 填入上一步创建的 `API Key`
   - `Resource ID` 保持默认 `volc.seedasr.sauc.duration`（豆包流式语音识别模型 2.0 · 小时版）

新旧两种模式共享同一 WebSocket 端点（`wss://openspeech.bytedance.com/api/v3/sauc/bigmodel_async`），仅握手鉴权头不同（新版为 `X-Api-Key` 单头）。官方接口文档：<https://www.volcengine.com/docs/6561/1354869>
