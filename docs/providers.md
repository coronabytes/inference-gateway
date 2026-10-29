# Providers

A **credential** is one provider account: where to reach it and how to authenticate. Add credentials under **Models → Credentials**. One credential serves any number of [deployments](models.md); rotating its key or changing its endpoint updates all of them at once. Two Azure resources or two AWS regions are two credentials. Secrets are encrypted when saved and never shown again.

| Provider | Serves | Connection | Authentication |
|---|---|---|---|
| OpenAI-compatible | Chat (Chat Completions or Responses), embeddings, rerank (Cohere's API) | Base URL (default: OpenAI). Works with vLLM, Ollama and other compatible servers. | API key (as `Authorization: Bearer`, or in a header you name), or none |
| Azure AI Foundry | Chat (Chat Completions, Responses, Anthropic Messages), embeddings, rerank (Cohere), decision models | Resource endpoint, optional API version | API key, or an Entra ID app registration (client credentials) |
| Amazon Bedrock | Chat (Converse for any Bedrock model; Claude's Anthropic Messages through InvokeModel or bedrock-mantle), embeddings, rerank (Amazon Rerank, Cohere Rerank) | Region, optional endpoint such as a VPC endpoint | IAM role through the AWS credential chain, access keys, or a Bedrock API key |
| Anthropic | Chat | Optional base URL | API key |
| Google Vertex AI | Chat (OpenAI-compatible for Google and Model Garden models, Anthropic Messages), embeddings | Global, EU or US multi-region, or a single region; optional project and endpoint | Service account key file, uploaded in the console |
| Google AI Studio | Chat, embeddings | Optional base URL | API key |
| Mistral AI | Chat, embeddings | Global, EU or US endpoint | API key |
| TypeSafe | Decision models | Optional base URL | API key |

**AWS credential chain.** A Bedrock credential without secrets uses the standard AWS sources: environment, profile, SSO, EKS (IRSA or Pod Identity), ECS task role or EC2 instance profile. The gateway then stores no AWS secret at all.

**Bedrock calls.** The gateway calls Bedrock through the official AWS SDK for Rust: Converse, InvokeModel (Claude's Messages API, Titan embeddings), CountTokens, ApplyGuardrail and the Agent Runtime's Rerank. Their connections go through the same network guard as every other upstream, and the SDK does not retry, since the gateway fails over to another deployment itself. For Claude, the Anthropic Messages protocol offers InvokeModel, which works in every region and inference profile that serves the model, or bedrock-mantle, Bedrock's Anthropic-compatible endpoint, which no SDK covers and which the gateway signs itself. Rerank with a Bedrock API key also goes without the SDK, whose Agent Runtime client cannot send one.

**Bedrock permissions.** Model calls need `bedrock:InvokeModel` and `bedrock:InvokeModelWithResponseStream` (Converse uses them too), rerank models also `bedrock:Rerank`, and token counting for Claude through InvokeModel `bedrock:CountTokens`; for an inference profile, on the profile and on its model in every region it routes to. A [guardrail](guardrails.md) using the credential needs `bedrock:ApplyGuardrail`. When you add a deployment with a credential that has AWS credentials (not a Bedrock API key), the console lists the models and inference profiles of its region (`bedrock:ListFoundationModels`, `bedrock:ListInferenceProfiles`) and fills in the upstream model, base model with its prices, and the RPM and TPM limits of the account's quota for it (`servicequotas:ListServiceQuotas`). Without these permissions the console says which one is missing and you type the values in. Bedrock quotas belong to the account and region, not to one deployment, so lower the limits when other deployments or applications use the same model.

**Regions.** Bedrock, Vertex AI and Mistral credentials pin traffic to a region or geography, for example to keep data in the EU.

**Private networks.** Models on your own network (a vLLM or Ollama server, an Azure private endpoint, a Bedrock VPC endpoint) need their network listed in `GATEWAY_UPSTREAM_ALLOWED_NETWORKS`; by default the gateway only connects to public addresses. See [Configuration](configuration.md#access-and-security).

**Entra ID for Azure.** Instead of the resource's key, an Azure credential can use an app registration: its tenant ID, client ID and a client secret. The gateway fetches an access token with the client credentials grant and renews it before it expires. Give the app **Cognitive Services OpenAI User** (Azure OpenAI models) or **Cognitive Services User** (all Foundry models, including Claude and Cohere) on the resource. Cognitive Services User also lets the gateway read the resource's deployments from Azure Resource Manager (with only Cognitive Services OpenAI User, add **Reader**), so when you add a deployment with such a credential the console offers the resource's deployments and fills in the deployment name, protocol, base model with its prices, and the RPM and TPM limits of its quota.
