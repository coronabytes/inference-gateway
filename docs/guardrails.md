# Guardrails

A **guardrail** screens prompts before they reach any model: it masks what must not leave, such as names, phone numbers and addresses, and blocks what must not pass, such as prompt attacks and denied topics. It works for every model behind the gateway, whichever provider screens the prompt.

## Providers

- **Amazon Bedrock Guardrails**, through its standalone `ApplyGuardrail` API: masks and blocks. Create the guardrail in the AWS console (Bedrock → Guardrails) and choose there what each policy does: block, mask (`{NAME}`, `{PHONE}`, …) or only detect. The credential needs `bedrock:ApplyGuardrail` on the guardrail.
- **Google Cloud Model Armor**, with a Vertex AI credential: blocks prompt injection and jailbreaks, harmful content, malicious URLs and CSAM, and finds sensitive data, which a de-identify template masks (`[PHONE_NUMBER]`, …). Enable the Model Armor API, give the service account the Model Armor User role, and create a template in Security → Model Armor.
- **Mistral's moderation API**, with a Mistral credential: scores the prompt in categories (jailbreaking, personal data, dangerous, criminal, violence, hate, sexual, self-harm, and health, financial and legal advice). It cannot mask. Nothing is set up at Mistral.

## Set up

On **Models → Guardrails → New guardrail**:

1. A name, the provider and a credential. For Bedrock, the guardrail's id or ARN and version (a number or `DRAFT`). For Model Armor, the template's id and location, and the filters that block. For Mistral, the moderation model, the categories that block and, optionally, a score threshold.
2. Prices are filled in with the provider's list prices and can be changed. They count toward key and project budgets.
3. **Try** (the flask) checks a text and shows what was masked or blocked, what was found, and the cost.

## Apply

- **On a project or a key**, in its editor on the **Access** page: every request of the key, or of every key in the project, goes through it. Only admins choose them; project users cannot take a guardrail off their own keys.
- **Per request:** a client asks for guardrails by name in the `x-gateway-guardrails` header, on top of those of its key. An unknown name is refused with `400`.

## What happens to a request

- The prompt's system, user and tool texts are checked on all three chat APIs, before routing, so auto routers decide on the screened prompt too.
- **Masked:** the request goes on with the masked text, whichever deployment serves it, and conversations are stored masked. The response carries `x-gateway-guardrail: masked`, and `x-gateway-guardrail-prompt` holds the prompt's last text as the model got it (base64, left out above 8 KB). The playground shows it as **Sent as**.
- **Blocked:** the request is refused with `400` and the guardrail's message, with `code: content_filter` in the OpenAI APIs, before any model is called.
- **Flagged** and **passed** requests go on unchanged, with `x-gateway-guardrail` set accordingly.
- A guardrail that cannot be reached refuses the request (`503`) rather than letting it through unscreened.
- The request log shows what the guardrails did, the kinds of things found (`pii:NAME`, `content:PROMPT_ATTACK`, never the matched text) and their cost.

A guardrail sends the prompt's text to its provider: for Bedrock in the guardrail's region, for Model Armor in the template's location, for Mistral in the credential's.

**Not yet supported:** guardrails on responses, and on embedding, rerank and decision requests.
