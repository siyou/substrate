(function() {var implementors = {};
implementors["sc_client_api"] = [{"text":"impl&lt;Block:&nbsp;<a class=\"trait\" href=\"sp_runtime/traits/trait.Block.html\" title=\"trait sp_runtime::traits::Block\">BlockT</a>&gt; HeaderMetadata&lt;Block&gt; for <a class=\"struct\" href=\"sc_client_api/in_mem/struct.Blockchain.html\" title=\"struct sc_client_api::in_mem::Blockchain\">Blockchain</a>&lt;Block&gt;","synthetic":false,"types":["sc_client_api::in_mem::Blockchain"]}];
implementors["sc_client_db"] = [{"text":"impl&lt;Block:&nbsp;<a class=\"trait\" href=\"sp_runtime/traits/trait.Block.html\" title=\"trait sp_runtime::traits::Block\">BlockT</a>&gt; <a class=\"trait\" href=\"sp_blockchain/header_metadata/trait.HeaderMetadata.html\" title=\"trait sp_blockchain::header_metadata::HeaderMetadata\">HeaderMetadata</a>&lt;Block&gt; for <a class=\"struct\" href=\"sc_client_db/struct.BlockchainDb.html\" title=\"struct sc_client_db::BlockchainDb\">BlockchainDb</a>&lt;Block&gt;","synthetic":false,"types":["sc_client_db::BlockchainDb"]}];
implementors["sc_service"] = [{"text":"impl&lt;B, E, Block, RA&gt; <a class=\"trait\" href=\"sp_blockchain/header_metadata/trait.HeaderMetadata.html\" title=\"trait sp_blockchain::header_metadata::HeaderMetadata\">HeaderMetadata</a>&lt;Block&gt; for <a class=\"struct\" href=\"sc_service/client/struct.Client.html\" title=\"struct sc_service::client::Client\">Client</a>&lt;B, E, Block, RA&gt; <span class=\"where fmt-newline\">where<br>&nbsp;&nbsp;&nbsp;&nbsp;B: <a class=\"trait\" href=\"sc_client_api/backend/trait.Backend.html\" title=\"trait sc_client_api::backend::Backend\">Backend</a>&lt;Block&gt;,<br>&nbsp;&nbsp;&nbsp;&nbsp;E: <a class=\"trait\" href=\"sc_client_api/call_executor/trait.CallExecutor.html\" title=\"trait sc_client_api::call_executor::CallExecutor\">CallExecutor</a>&lt;Block&gt;,<br>&nbsp;&nbsp;&nbsp;&nbsp;Block: <a class=\"trait\" href=\"sp_runtime/traits/trait.Block.html\" title=\"trait sp_runtime::traits::Block\">BlockT</a>,&nbsp;</span>","synthetic":false,"types":["sc_service::client::client::Client"]}];
implementors["substrate_test_runtime_transaction_pool"] = [{"text":"impl <a class=\"trait\" href=\"sp_blockchain/header_metadata/trait.HeaderMetadata.html\" title=\"trait sp_blockchain::header_metadata::HeaderMetadata\">HeaderMetadata</a>&lt;<a class=\"struct\" href=\"sp_runtime/generic/block/struct.Block.html\" title=\"struct sp_runtime::generic::block::Block\">Block</a>&lt;<a class=\"struct\" href=\"sp_runtime/generic/header/struct.Header.html\" title=\"struct sp_runtime::generic::header::Header\">Header</a>&lt;<a class=\"primitive\" href=\"https://doc.rust-lang.org/nightly/std/primitive.u64.html\">u64</a>, <a class=\"struct\" href=\"sp_runtime/traits/struct.BlakeTwo256.html\" title=\"struct sp_runtime::traits::BlakeTwo256\">BlakeTwo256</a>&gt;, <a class=\"enum\" href=\"substrate_test_runtime/enum.Extrinsic.html\" title=\"enum substrate_test_runtime::Extrinsic\">Extrinsic</a>&gt;&gt; for <a class=\"struct\" href=\"substrate_test_runtime_transaction_pool/struct.TestApi.html\" title=\"struct substrate_test_runtime_transaction_pool::TestApi\">TestApi</a>","synthetic":false,"types":["substrate_test_runtime_transaction_pool::TestApi"]}];
if (window.register_implementors) {window.register_implementors(implementors);} else {window.pending_implementors = implementors;}})()