# Providers

A **credential** is one provider account: where to reach it and how to authenticate. Add credentials under **Models → Credentials**. One credential serves any number of [deployments](models.md); rotating its key or changing its endpoint updates all of them at once. Two Azure resources or two AWS regions are two credentials. Secrets are encrypted when saved and never shown again.

| Provider | Serves | Connection | Authentication |
|---|---|---|---|
| OpenAI-compatible | Chat (Chat Completions or Responses), embeddings, rerank (Cohere's API) | Base URL (default: OpenAI). Works with vLLM, Ollama and other compatible servers. | API key (as `Authorization: Bearer`, or in a header you name), or none |
| Azure AI Foundry | Chat (Chat Completions, Responses, Anthropic Messages), embeddings, rerank (Cohere), decision models | Resource endpoint, optional API version | API key |
| Amazon Bedrock | Chat (Converse for any Bedrock model, Anthropic Messages), embeddings, rerank (Amazon Rerank, Cohere Rerank) | Region, optional endpoint such as a VPC endpoint | IAM role through the AWS credential chain, access keys, or a Bedrock API key |
| Anthropic | Chat | Optional base URL | API key |
| Google Vertex AI | Chat (OpenAI-compatible for Google and Model Garden models, Anthropic Messages), embeddings | Global, EU or US multi-region, or a single region; optional project and endpoint | Service account key file, uploaded in the console |
| Google AI Studio | Chat, embeddings | Optional base URL | API key |
| Mistral AI | Chat, embeddings | Global, EU or US endpoint | API key |
| TypeSafe | Decision models | Optional base URL | API key |

**AWS credential chain.** A Bedrock credential without secrets uses the standard AWS sources: environment, profile, SSO, EKS (IRSA or Pod Identity), ECS task role or EC2 instance profile. The gateway then stores no AWS secret at all.

**Regions.** Bedrock, Vertex AI and Mistral credentials pin traffic to a region or geography, for example to keep data in the EU.

**Private networks.** Models on your own network (a vLLM or Ollama server, an Azure private endpoint, a Bedrock VPC endpoint) need their network listed in `GATEWAY_UPSTREAM_ALLOWED_NETWORKS`; by default the gateway only connects to public addresses. See [Configuration](configuration.md#access-and-security).

**Not yet supported:** Azure Entra ID tokens for Azure credentials (use an API key).
