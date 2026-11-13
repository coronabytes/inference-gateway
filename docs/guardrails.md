# Guardrails

A **guardrail** screens prompts before they reach any model: it masks what must not leave, such as names, phone numbers and addresses, and blocks what must not pass, such as prompt attacks and denied topics. It works for every model behind the gateway, whichever provider screens the prompt.

## Providers

- **Amazon Bedrock Guardrails**, through its standalone `ApplyGuardrail` API: masks and blocks. Create the guardrail in the AWS console (Bedrock → Guardrails) and choose there what each policy does: block, mask (`{NAME}`, `{PHONE}`, …) or only detect. The credential needs `bedrock:ApplyGuardrail` on the guardrail.
- **Google Cloud Model Armor**, with a Vertex AI credential: blocks prompt injection and jailbreaks, harmful content, malicious URLs and CSAM, and finds sensitive data, which a de-identify template masks (`[PHONE_NUMBER]`, …). Enable the Model Armor API, give the service account the Model Armor User role, and create a template in Security → Model Armor.
- **Mistral's moderation API**, with a Mistral credential: scores the prompt in categories (jailbreaking, personal data, dangerous, criminal, violence, hate, sexual, self-harm, and health, financial and legal advice). It cannot mask. Nothing is set up at Mistral.
- **Presidio**, self-hosted, with a [generic HTTP credential](providers.md) holding the analyzer's address and its authentication (none, Basic or Bearer, for the proxy in front of it): finds personal data (`PERSON`, `EMAIL_ADDRESS`, `IBAN_CODE`, …). The guardrail chooses which entity types are masked (`<PERSON>`) and which block; the others are flagged. The gateway masks the text itself, so only the analyzer is needed, not Presidio's anonymizer. Nothing is billed.

## Set up

On **Models → Guardrails → New guardrail**:

1. A name, the provider and a credential. For Bedrock, the guardrail's id or ARN and version (a number or `DRAFT`). For Model Armor, the template's id and location, and the filters that block. For Mistral, the moderation model, the categories that block and, optionally, a score threshold. For Presidio, its languages, the entity types it masks and those it blocks, and optionally a score threshold. The console offers only the languages the analyzer has a model for, and the entity types it finds in them.
2. Prices are filled in with the provider's list prices and can be changed. They count toward key and project budgets. Presidio has none.
3. **Try** (the flask) checks a text and shows what was masked or blocked, what was found, and the cost.

## Presidio's languages

Presidio does not detect languages: its analyzer is asked in one, and answers only for languages it has a model for (its stock image: English only).

- **One language:** every text is analyzed in it. Any language the analyzer has a model for.
- **Several:** the gateway detects each text's language, in-process with [Lingua](https://github.com/pemistahl/lingua-rs), among the guardrail's languages only, and asks the analyzer in that one. A system prompt in English and a question in German are each analyzed in their own language. A text too short or too mixed to tell is analyzed in the first language.

The gateway detects these languages: English, Finnish, French, German, Italian, Norwegian (Bokmål), Polish, Spanish and Swedish. A guardrail with several languages lists only these; one with a single language can use any other.

For each language other than English, configure the analyzer with a spaCy model for it, such as `de_core_news_lg`, and register its pattern recognizers for that language: most of Presidio's predefined recognizers are English-only, so without that a German analyzer finds little beyond names, places and organisations. Presidio has no list of its languages, so the console asks the analyzer's `/supportedentities` about every ISO 639-1 code, and offers those it answers for. A proxy in front of the analyzer must pass `/supportedentities` as well as `/analyze`; otherwise the console offers the languages the gateway detects and Presidio's predefined entity types. A guardrail that lists a language the analyzer has no model for refuses the requests in it.

## Pseudonyms

A Presidio guardrail masks what it finds in one of three ways (**Masked as** in its editor):

- **`<PERSON>`**: the entity type. The model cannot tell two people apart.
- **Person A**: a pseudonym, the same one for the same value throughout the request, so the model can still tell who is who. Alice is `Person A`, Bob `Person B`, Berlin `Location A`; values that differ only in case or spacing are the same. Letters are handed out in the order the values first appear, and a client sends the conversation so far with every request, so a value keeps its letter turn after turn.
- **Person A, restored**: the answer gets the real values back. The client reads Alice where the model wrote `Person A`, in the text and in tool calls' arguments, streamed or not; reasoning stays as the model wrote it. The model and the request log only ever see the pseudonyms. The assistant's earlier turns, which the client got back with the real values, are pseudonymized again before the model sees them.

A pseudonymizing guardrail screens every text of every request: its pseudonyms depend on the whole request, so it does not remember earlier verdicts. With a conversation stored at the gateway or the provider (`previous_response_id` in the Responses API), the earlier turns are not in the request, and its letters start again with each turn; send the conversation in full to keep them.

## Apply

- **On a project or a key**, in its editor on the **Access** page: every request of the key, or of every key in the project, goes through it. Only admins choose them; project users cannot take a guardrail off their own keys.
- **Per request:** a client asks for guardrails by name in the `x-gateway-guardrails` header, on top of those of its key. An unknown name is refused with `400`.

## What happens to a request

- The prompt's system, user and tool texts are checked on all three chat APIs, before routing, so auto routers decide on the screened prompt too.
- **Masked:** the request goes on with the masked text, whichever deployment serves it, and conversations are stored masked. With restored [pseudonyms](#pseudonyms), the answer carries the real values. The response carries `x-gateway-guardrail: masked`, and `x-gateway-guardrail-prompt` holds the prompt's last text as the model got it (base64, left out above 8 KB). The playground shows it as **Sent as**.
- **Blocked:** the request is refused with `400` and the guardrail's message, with `code: content_filter` in the OpenAI APIs, before any model is called.
- **Flagged** and **passed** requests go on unchanged, with `x-gateway-guardrail` set accordingly.
- A guardrail that cannot be reached refuses the request (`503`) rather than letting it through unscreened.
- The request log shows what the guardrails did, the kinds of things found (`pii:NAME`, `content:PROMPT_ATTACK`, never the matched text) and their cost. For keys with [prompt logging](security.md#prompt-logging) on, the stored bodies also keep each guardrail's findings with the matched text, the texts it changed, and its provider's answer.

A guardrail sends the prompt's text to its provider: for Bedrock in the guardrail's region, for Model Armor in the template's location, for Mistral in the credential's, for Presidio to the analyzer's address.

**Not yet supported:** guardrails on responses, and on embedding, rerank and decision requests.
