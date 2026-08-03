import { LLM_PRESETS } from './ProvidersSection';
import { ASR_PRESETS } from './shared';

const atlascloudPreset = LLM_PRESETS.find(p => p.id === 'atlascloud');

if (!atlascloudPreset) {
  throw new Error('Atlas Cloud LLM preset is missing');
}

if (atlascloudPreset.baseUrl !== 'https://api.atlascloud.ai/v1') {
  throw new Error(`unexpected Atlas Cloud base URL: ${atlascloudPreset.baseUrl}`);
}

if (atlascloudPreset.modelPlaceholder !== 'qwen/qwen3.5-flash') {
  throw new Error(`unexpected Atlas Cloud default model: ${atlascloudPreset.modelPlaceholder}`);
}

const openAiCompatiblePreset = ASR_PRESETS.find(p => p.id === 'openai-compatible');

if (!openAiCompatiblePreset) {
  throw new Error('Custom OpenAI-compatible ASR preset is missing');
}

if (openAiCompatiblePreset.baseUrl !== '' || openAiCompatiblePreset.model !== '') {
  throw new Error(
    `Custom OpenAI-compatible ASR preset must have no defaults (got baseUrl=${openAiCompatiblePreset.baseUrl}, model=${openAiCompatiblePreset.model})`,
  );
}
