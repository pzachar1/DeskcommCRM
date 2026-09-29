//! QUEM PEDIU PARA SAIR — a regra, num lugar só.
//!
//! Porte da regra do DeskcommCRM (`lib/opt-out/deteccao.ts`) para o Rust, com o
//! mesmo formato de decisão e sem dependência de regex (o Worker paga o tamanho
//! do binário). O que ela NÃO faz é o ponto: **não caça a palavra solta no meio
//! da frase.** Medido em produção numa clínica, a regra antiga do Deskcomm
//! bloqueava "tem como parar a dor?" e "posso sair antes das 15h?" — 12 falsos
//! positivos em 32 frases de nicho — e falhava calada: a pessoa sumia da
//! conversa com um motivo que parecia legítimo (`stop_keyword`).
//!
//! Só bloqueia em dois casos:
//!
//! 1. **palavra ISOLADA** — a mensagem inteira é a palavra ("STOP", "sair",
//!    "baja"). É a convenção do canal, e é o que a própria plantilla aprovada
//!    promete em espanhol ("Respondé BAJA para no recibir más").
//! 2. **verbo de cessação + OBJETO DE COMUNICAÇÃO** — "parar de me mandar",
//!    "não quero mais receber", "sair da lista". Com a exceção que o Deskcomm
//!    pagou duas vezes: se o objeto é pedido, entrega, boleto ou fatura
//!    ("pare de mandar o pedido nesse endereço"), NÃO é descadastro — é um
//!    cliente que quer continuar sendo atendido sobre outro assunto.
//!
//! Dois níveis, e a diferença é de política:
//!
//! - [`pedido_de_opt_out`] (inequívoco) é o que autoriza gravar `is_blocked`,
//!   estado que só uma pessoa desfaz.
//! - [`opt_out_provavel`] soma o ambíguo ("me deixa em paz", "chega") e serve ao
//!   agente: parar de responder e chamar humano, que confirma o bloqueio.
//!   Deixar o ambíguo bloquear sozinho inverteria a política.

/// Minúsculas e sem acento — a forma sobre a qual todo padrão daqui roda.
pub fn normalizar(texto: &str) -> String {
    texto
        .chars()
        .flat_map(|c| {
            let c = c.to_lowercase().next().unwrap_or(c);
            let plain = match c {
                'á' | 'à' | 'ã' | 'â' | 'ä' | 'å' => 'a',
                'é' | 'è' | 'ê' | 'ë' => 'e',
                'í' | 'ì' | 'î' | 'ï' => 'i',
                'ó' | 'ò' | 'õ' | 'ô' | 'ö' => 'o',
                'ú' | 'ù' | 'û' | 'ü' => 'u',
                'ç' => 'c',
                'ñ' => 'n',
                other => other,
            };
            Some(plain)
        })
        .collect()
}

/// Palavras da frase normalizada, sem pontuação: "STOP." e "SAIR!" contam.
fn palavras(normalizado: &str) -> Vec<&str> {
    normalizado
        .split(|c: char| !c.is_alphanumeric())
        .filter(|p| !p.is_empty())
        .collect()
}

/// Palavra-chave que só vale ENVIADA SOZINHA (mensagem inteira = a palavra).
pub const PALAVRAS_DE_OPT_OUT: &[&str] = &[
    "stop",
    "parar",
    "pare",
    "sair",
    "cancelar",
    "descadastrar",
    "remover",
    "unsubscribe",
    // espanhol: `baja` é a palavra que a plantilla aprovada pede
    "baja",
    "bajar",
    "salir",
    "desuscribir",
    "desuscribirme",
];

/// Verbos de cessação: "pare de...", "deja de...".
const VERBOS_DE_CESSACAO: &[&str] = &["parar", "para", "pare", "parem", "paren", "dejar", "deja", "deje", "dejen", "cessar", "cesse"];

/// Verbos que revelam que o objeto do pedido é a COMUNICAÇÃO, e não o
/// tratamento, a dor, o horário ou o trabalho da pessoa.
const VERBOS_DE_COMUNICACAO: &[&str] = &[
    "mandar", "manda", "mande", "mandem", "mandes", "enviar", "envia", "envie", "enviem", "envien", "receber", "recebe", "escrever",
    "escreve", "chamar", "chama", "ligar", "liga", "perturbar", "perturba", "encher", "enche", "insistir", "insiste", "recibir",
    "recibe", "escribir", "escribe", "escriban", "escribirme", "molestar", "molesta", "llamar", "llama", "contactar", "contacta",
    "contacte", "contacten", "contactes", "contactarme", "mandarme", "enviarme", "escreverme",
];

/// Objetos que vêm depois de um verbo de comunicação mas NÃO são a comunicação:
/// pedido, entrega, boleto, fatura, campanha. Sem isto, "pare de mandar o pedido
/// nesse endereço" bloqueia um cliente que quer continuar sendo atendido.
const OBJETOS_NAO_COMUNICATIVOS: &[&str] = &[
    "pedido", "pedidos", "encomenda", "encomendas", "pacote", "pacotes", "entrega", "entregas", "fatura", "faturas", "boleto",
    "boletos", "cobranca", "cobrancas", "produto", "produtos", "paquete", "paquetes", "envio", "envios", "factura", "facturas",
    "boleta", "boletas", "cobro", "cobros", "producto", "productos", "pauta", "pautas", "presupuesto", "presupuestos",
];

/// Palavras que podem aparecer entre o verbo e o objeto: "o pedido", "el paquete",
/// "me mandar", "de receber".
const LIGACOES: &[&str] = &[
    "de", "me", "mas", "mais", "nos", "nas", "o", "a", "os", "as", "el", "la", "los", "las", "lo", "meu", "minha", "meus", "minhas",
    "seu", "sua", "seus", "suas", "mi", "mis", "tu", "tus", "esse", "essa", "esses", "essas", "ese", "esa", "esos", "esas", "nesse",
    "nessa", "para", "pra",
];

fn tem(lista: &[&str], p: &str) -> bool {
    lista.contains(&p)
}

/// Depois de um verbo de comunicação, o próximo substantivo é um objeto que NÃO é
/// comunicação? ("mandar o pedido"). Pula as ligações no caminho.
fn objeto_nao_comunicativo(resto: &[&str]) -> bool {
    resto
        .iter()
        .skip_while(|p| tem(LIGACOES, p))
        .next()
        .is_some_and(|p| tem(OBJETOS_NAO_COMUNICATIVOS, p))
}

/// Existe um verbo de cessação seguido (com ligações no meio) de um verbo de
/// comunicação cujo objeto é a própria comunicação?
fn cessacao_de_comunicacao(ps: &[&str]) -> bool {
    for (i, p) in ps.iter().enumerate() {
        if !tem(VERBOS_DE_CESSACAO, p) {
            continue;
        }
        // até 3 palavras de ligação entre "pare" e "mandar"
        let mut j = i + 1;
        let fim = (i + 4).min(ps.len());
        while j < fim && tem(LIGACOES, ps[j]) {
            j += 1;
        }
        if j < ps.len() && tem(VERBOS_DE_COMUNICACAO, ps[j]) && !objeto_nao_comunicativo(&ps[j + 1..]) {
            return true;
        }
    }
    false
}

/// "não quero (mais) receber" / "no quiero recibir (mas)" — com a ressalva de
/// canal: "não quero receber ligação, só whatsapp" é TROCA de canal, não saída.
fn nao_quero_receber(ps: &[&str]) -> bool {
    const NEGACOES: &[&str] = &["nao", "no"];
    const QUERER: &[&str] = &["quero", "queria", "quiero", "desejo", "deseo", "gostaria"];
    const CANAIS_DE_VOZ: &[&str] = &["ligacao", "ligacoes", "chamada", "chamadas", "telefonema", "telefonemas", "telefone", "llamada", "llamadas"];
    for (i, p) in ps.iter().enumerate() {
        if !tem(NEGACOES, p) || i + 1 >= ps.len() {
            continue;
        }
        let mut j = i + 1;
        if !tem(QUERER, ps[j]) {
            continue;
        }
        j += 1;
        let fim = (j + 3).min(ps.len());
        while j < fim && tem(LIGACOES, ps[j]) {
            j += 1;
        }
        if j >= ps.len() {
            continue;
        }
        // "não quero mais mensagem / contato / nada"
        if tem(&["mensagem", "mensagens", "mensaje", "mensajes", "contato", "contatos", "contacto", "nada", "propaganda", "publicidade"], ps[j]) {
            return true;
        }
        if tem(VERBOS_DE_COMUNICACAO, ps[j]) {
            let resto = &ps[j + 1..];
            let proximo = resto.iter().skip_while(|p| tem(LIGACOES, p)).next();
            if proximo.is_some_and(|p| tem(CANAIS_DE_VOZ, p)) || objeto_nao_comunicativo(resto) {
                continue;
            }
            return true;
        }
    }
    false
}

/// Frases feitas de descadastro, no texto normalizado. Todas nomeiam o objeto
/// (lista, inscrição, assinatura) — nenhuma casa palavra solta.
const FRASES_INEQUIVOCAS: &[&str] = &[
    "sair da lista",
    "sair dessa lista",
    "sair desta lista",
    "me tira da lista",
    "me tire da lista",
    "me tirem da lista",
    "me remove da lista",
    "me remova da lista",
    "me retire da lista",
    "me exclui da lista",
    "me exclua da lista",
    "tira meu numero",
    "tire meu numero",
    "apaga meu numero",
    "apague meu numero",
    "cancelar a inscricao",
    "cancelar inscricao",
    "cancelar a assinatura",
    "cancelar assinatura",
    "dar de baja",
    "darme de baja",
    "quiero darme de baja",
    "borrame de la lista",
    "sacame de la lista",
    "eliminame de la lista",
    "salir de la lista",
    "cancelar la suscripcion",
    "no me contacten mas",
    "no me escriban mas",
    "no me manden mas",
];

/// Ambíguo: sinal de que a conversa acabou, sem ser pedido formal de saída.
/// Serve para o agente calar a boca e chamar humano — nunca para bloquear.
const FRASES_AMBIGUAS: &[&str] = &[
    "me deixa em paz",
    "me deixe em paz",
    "deixa eu em paz",
    "chega disso",
    "ja chega",
    "para de me encher",
    "nao me incomode",
    "nao me perturbe",
    "dejame en paz",
    "dejame tranquilo",
    "basta ya",
    "no me molestes",
    "no insistan",
    "nao insista",
];

/// Pedido INEQUÍVOCO de descadastro: é isto — e só isto — que autoriza gravar
/// `contacts.is_blocked`.
pub fn pedido_de_opt_out(texto: &str) -> bool {
    let n = normalizar(texto);
    let ps = palavras(&n);
    if ps.is_empty() {
        return false;
    }
    // 1. a palavra SOZINHA
    if ps.len() == 1 && tem(PALAVRAS_DE_OPT_OUT, ps[0]) {
        return true;
    }
    // 2. frase feita, ou verbo de cessação com objeto de comunicação
    if FRASES_INEQUIVOCAS.iter().any(|f| n.contains(f)) {
        return true;
    }
    if ps.iter().any(|p| p.starts_with("descadastr") || p.starts_with("desinscrev") || p.starts_with("desuscrib")) {
        return true;
    }
    cessacao_de_comunicacao(&ps) || nao_quero_receber(&ps)
}

/// Inequívoco OU ambíguo. O agente para de responder nos dois casos; só o
/// inequívoco bloqueia o contato.
pub fn opt_out_provavel(texto: &str) -> bool {
    if pedido_de_opt_out(texto) {
        return true;
    }
    let n = normalizar(texto);
    FRASES_AMBIGUAS.iter().any(|f| n.contains(f))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frases de controle: as de nicho que a regra antiga do Deskcomm bloqueava
    /// por engano continuam PASSANDO, e os pedidos de verdade continuam pegando.
    #[test]
    fn palavra_no_meio_da_frase_nao_bloqueia() {
        for frase in [
            "tem como parar a dor?",
            "posso sair antes das 15h?",
            "preciso sair mais cedo da consulta",
            "vou parar de fumar, isso atrapalha o tratamento?",
            "quero cancelar minha consulta de quinta",
            "Voy a salir ahora",
            "Doy de baja la pauta?",
            "pare de mandar o pedido nesse endereco, mudei de casa",
            "nao me mande mais boletos, ja paguei",
            "nao quero receber ligacao, so whatsapp",
        ] {
            assert!(!pedido_de_opt_out(frase), "bloqueou por engano: {frase}");
        }
    }

    #[test]
    fn palavra_sozinha_bloqueia() {
        for frase in ["STOP", "sair", "Parar.", "CANCELAR!", "baja", "Salir", "unsubscribe"] {
            assert!(pedido_de_opt_out(frase), "deixou passar: {frase}");
        }
    }

    #[test]
    fn pedido_por_extenso_bloqueia() {
        for frase in [
            "pare de me mandar mensagem",
            "para de mandar mensagens por favor",
            "nao quero mais receber nada de voces",
            "nao quero receber mais mensagens",
            "me tira da lista",
            "quero sair da lista",
            "cancelar a inscricao",
            "me descadastrar",
            "quiero darme de baja",
            "no quiero recibir mas mensajes",
            "borrame de la lista",
            "deja de escribirme",
        ] {
            assert!(pedido_de_opt_out(frase), "deixou passar: {frase}");
        }
    }

    #[test]
    fn ambiguo_nao_bloqueia_mas_cala_o_agente() {
        for frase in ["me deixa em paz", "ja chega", "dejame en paz", "no me molestes"] {
            assert!(!pedido_de_opt_out(frase), "ambíguo não pode bloquear: {frase}");
            assert!(opt_out_provavel(frase), "ambíguo tem de calar o agente: {frase}");
        }
    }

    #[test]
    fn conversa_comum_nao_dispara_nada() {
        for frase in [
            "oi, bom dia! queria saber o preco",
            "obrigado, era isso",
            "pode me mandar o endereco?",
            "qual o horario de amanha?",
        ] {
            assert!(!opt_out_provavel(frase), "disparou por engano: {frase}");
        }
    }
}
