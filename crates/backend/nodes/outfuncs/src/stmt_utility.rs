//! outfuncs.funcs.c writers for utility statements, DDL, and raw parse nodes.

use super::*;
use super::stmt_plan::{out_func_call, out_variable_set_stmt};
use types_nodes::parsenodes::*;
use types_nodes::rawnodes::*;

pub(super) fn try_out_node(out: &mut PgString<'_>, node: Node<'_>) -> Option<PgResult<()>> {
    macro_rules! arm {
        ($ty:ty, $f:ident) => {
            $f(out, node.as_variant::<$ty>().expect(stringify!($ty)))
        };
    }
    let mut handled = true;
    let r = (|| -> PgResult<()> {
        match node.node_tag() {
            NodeTag::T_IntoClause => arm!(IntoClause, out_into_clause),
            NodeTag::T_RoleSpec => arm!(RoleSpec, out_role_spec),
            NodeTag::T_RangeTableFunc => arm!(RangeTableFunc, out_range_table_func),
            NodeTag::T_RangeTableFuncCol => arm!(RangeTableFuncCol, out_range_table_func_col),
            NodeTag::T_RangeTableSample => arm!(RangeTableSample, out_range_table_sample),
            NodeTag::T_TableLikeClause => arm!(TableLikeClause, out_table_like_clause),
            NodeTag::T_IndexElem => arm!(IndexElem, out_index_elem),
            NodeTag::T_DefElem => arm!(DefElem, out_def_elem),
            NodeTag::T_XmlSerialize => arm!(XmlSerialize, out_xml_serialize),
            NodeTag::T_PartitionElem => arm!(PartitionElem, out_partition_elem),
            NodeTag::T_PartitionSpec => arm!(PartitionSpec, out_partition_spec),
            NodeTag::T_PartitionCmd => arm!(PartitionCmd, out_partition_cmd),
            NodeTag::T_MergeWhenClause => arm!(MergeWhenClause, out_merge_when_clause),
            NodeTag::T_TriggerTransition => arm!(TriggerTransition, out_trigger_transition),
            NodeTag::T_JsonOutput => arm!(JsonOutput, out_json_output),
            NodeTag::T_JsonArgument => arm!(JsonArgument, out_json_argument),
            NodeTag::T_JsonFuncExpr => arm!(JsonFuncExpr, out_json_func_expr),
            NodeTag::T_JsonTablePathSpec => arm!(JsonTablePathSpec, out_json_table_path_spec),
            NodeTag::T_JsonTable => arm!(JsonTable, out_json_table),
            NodeTag::T_JsonTableColumn => arm!(JsonTableColumn, out_json_table_column),
            NodeTag::T_JsonKeyValue => arm!(JsonKeyValue, out_json_key_value),
            NodeTag::T_JsonParseExpr => arm!(JsonParseExpr, out_json_parse_expr),
            NodeTag::T_JsonScalarExpr => arm!(JsonScalarExpr, out_json_scalar_expr),
            NodeTag::T_JsonSerializeExpr => arm!(JsonSerializeExpr, out_json_serialize_expr),
            NodeTag::T_JsonObjectConstructor => arm!(JsonObjectConstructor, out_json_object_constructor),
            NodeTag::T_JsonArrayConstructor => arm!(JsonArrayConstructor, out_json_array_constructor),
            NodeTag::T_JsonArrayQueryConstructor => arm!(JsonArrayQueryConstructor, out_json_array_query_constructor),
            NodeTag::T_JsonAggConstructor => arm!(JsonAggConstructor, out_json_agg_constructor),
            NodeTag::T_JsonObjectAgg => arm!(JsonObjectAgg, out_json_object_agg),
            NodeTag::T_JsonArrayAgg => arm!(JsonArrayAgg, out_json_array_agg),
            NodeTag::T_MergeStmt => arm!(MergeStmt, out_merge_stmt),
            NodeTag::T_ReturnStmt => arm!(ReturnStmt, out_return_stmt),
            NodeTag::T_PLAssignStmt => arm!(PLAssignStmt, out_pl_assign_stmt),
            NodeTag::T_CreateSchemaStmt => arm!(CreateSchemaStmt, out_create_schema_stmt),
            NodeTag::T_AlterTableStmt => arm!(AlterTableStmt, out_alter_table_stmt),
            NodeTag::T_AlterTableCmd => arm!(AlterTableCmd, out_alter_table_cmd),
            NodeTag::T_ATAlterConstraint => arm!(ATAlterConstraint, out_at_alter_constraint),
            NodeTag::T_ReplicaIdentityStmt => arm!(ReplicaIdentityStmt, out_replica_identity_stmt),
            NodeTag::T_AlterCollationStmt => arm!(AlterCollationStmt, out_alter_collation_stmt),
            NodeTag::T_AlterDomainStmt => arm!(AlterDomainStmt, out_alter_domain_stmt),
            NodeTag::T_GrantStmt => arm!(GrantStmt, out_grant_stmt),
            NodeTag::T_AccessPriv => arm!(AccessPriv, out_access_priv),
            NodeTag::T_GrantRoleStmt => arm!(GrantRoleStmt, out_grant_role_stmt),
            NodeTag::T_AlterDefaultPrivilegesStmt => arm!(AlterDefaultPrivilegesStmt, out_alter_default_privileges_stmt),
            NodeTag::T_CopyStmt => arm!(CopyStmt, out_copy_stmt),
            NodeTag::T_Constraint => arm!(Constraint, out_constraint),
            NodeTag::T_CreateTableSpaceStmt => arm!(CreateTableSpaceStmt, out_create_table_space_stmt),
            NodeTag::T_DropTableSpaceStmt => arm!(DropTableSpaceStmt, out_drop_table_space_stmt),
            NodeTag::T_AlterTableSpaceOptionsStmt => arm!(AlterTableSpaceOptionsStmt, out_alter_table_space_options_stmt),
            NodeTag::T_AlterTableMoveAllStmt => arm!(AlterTableMoveAllStmt, out_alter_table_move_all_stmt),
            NodeTag::T_CreateExtensionStmt => arm!(CreateExtensionStmt, out_create_extension_stmt),
            NodeTag::T_AlterExtensionStmt => arm!(AlterExtensionStmt, out_alter_extension_stmt),
            NodeTag::T_AlterExtensionContentsStmt => arm!(AlterExtensionContentsStmt, out_alter_extension_contents_stmt),
            NodeTag::T_CreateFdwStmt => arm!(CreateFdwStmt, out_create_fdw_stmt),
            NodeTag::T_AlterFdwStmt => arm!(AlterFdwStmt, out_alter_fdw_stmt),
            NodeTag::T_CreateForeignServerStmt => arm!(CreateForeignServerStmt, out_create_foreign_server_stmt),
            NodeTag::T_AlterForeignServerStmt => arm!(AlterForeignServerStmt, out_alter_foreign_server_stmt),
            NodeTag::T_CreateForeignTableStmt => arm!(CreateForeignTableStmt, out_create_foreign_table_stmt),
            NodeTag::T_CreateUserMappingStmt => arm!(CreateUserMappingStmt, out_create_user_mapping_stmt),
            NodeTag::T_AlterUserMappingStmt => arm!(AlterUserMappingStmt, out_alter_user_mapping_stmt),
            NodeTag::T_DropUserMappingStmt => arm!(DropUserMappingStmt, out_drop_user_mapping_stmt),
            NodeTag::T_ImportForeignSchemaStmt => arm!(ImportForeignSchemaStmt, out_import_foreign_schema_stmt),
            NodeTag::T_CreatePolicyStmt => arm!(CreatePolicyStmt, out_create_policy_stmt),
            NodeTag::T_AlterPolicyStmt => arm!(AlterPolicyStmt, out_alter_policy_stmt),
            NodeTag::T_CreateAmStmt => arm!(CreateAmStmt, out_create_am_stmt),
            NodeTag::T_CreateTrigStmt => arm!(CreateTrigStmt, out_create_trig_stmt),
            NodeTag::T_CreateEventTrigStmt => arm!(CreateEventTrigStmt, out_create_event_trig_stmt),
            NodeTag::T_AlterEventTrigStmt => arm!(AlterEventTrigStmt, out_alter_event_trig_stmt),
            NodeTag::T_CreatePLangStmt => arm!(CreatePLangStmt, out_create_p_lang_stmt),
            NodeTag::T_CreateRoleStmt => arm!(CreateRoleStmt, out_create_role_stmt),
            NodeTag::T_AlterRoleStmt => arm!(AlterRoleStmt, out_alter_role_stmt),
            NodeTag::T_AlterRoleSetStmt => arm!(AlterRoleSetStmt, out_alter_role_set_stmt),
            NodeTag::T_DropRoleStmt => arm!(DropRoleStmt, out_drop_role_stmt),
            NodeTag::T_CreateSeqStmt => arm!(CreateSeqStmt, out_create_seq_stmt),
            NodeTag::T_AlterSeqStmt => arm!(AlterSeqStmt, out_alter_seq_stmt),
            NodeTag::T_DefineStmt => arm!(DefineStmt, out_define_stmt),
            NodeTag::T_CreateDomainStmt => arm!(CreateDomainStmt, out_create_domain_stmt),
            NodeTag::T_CreateOpClassStmt => arm!(CreateOpClassStmt, out_create_op_class_stmt),
            NodeTag::T_CreateOpClassItem => arm!(CreateOpClassItem, out_create_op_class_item),
            NodeTag::T_CreateOpFamilyStmt => arm!(CreateOpFamilyStmt, out_create_op_family_stmt),
            NodeTag::T_AlterOpFamilyStmt => arm!(AlterOpFamilyStmt, out_alter_op_family_stmt),
            NodeTag::T_DropStmt => arm!(DropStmt, out_drop_stmt),
            NodeTag::T_TruncateStmt => arm!(TruncateStmt, out_truncate_stmt),
            NodeTag::T_CommentStmt => arm!(CommentStmt, out_comment_stmt),
            NodeTag::T_SecLabelStmt => arm!(SecLabelStmt, out_sec_label_stmt),
            NodeTag::T_DeclareCursorStmt => arm!(DeclareCursorStmt, out_declare_cursor_stmt),
            NodeTag::T_ClosePortalStmt => arm!(ClosePortalStmt, out_close_portal_stmt),
            NodeTag::T_FetchStmt => arm!(FetchStmt, out_fetch_stmt),
            NodeTag::T_IndexStmt => arm!(IndexStmt, out_index_stmt),
            NodeTag::T_CreateStatsStmt => arm!(CreateStatsStmt, out_create_stats_stmt),
            NodeTag::T_StatsElem => arm!(StatsElem, out_stats_elem),
            NodeTag::T_AlterStatsStmt => arm!(AlterStatsStmt, out_alter_stats_stmt),
            NodeTag::T_CreateFunctionStmt => arm!(CreateFunctionStmt, out_create_function_stmt),
            NodeTag::T_AlterFunctionStmt => arm!(AlterFunctionStmt, out_alter_function_stmt),
            NodeTag::T_DoStmt => arm!(DoStmt, out_do_stmt),
            NodeTag::T_CallStmt => arm!(CallStmt, out_call_stmt),
            NodeTag::T_RenameStmt => arm!(RenameStmt, out_rename_stmt),
            NodeTag::T_AlterObjectSchemaStmt => arm!(AlterObjectSchemaStmt, out_alter_object_schema_stmt),
            NodeTag::T_AlterOwnerStmt => arm!(AlterOwnerStmt, out_alter_owner_stmt),
            NodeTag::T_AlterOperatorStmt => arm!(AlterOperatorStmt, out_alter_operator_stmt),
            NodeTag::T_AlterTypeStmt => arm!(AlterTypeStmt, out_alter_type_stmt),
            NodeTag::T_RuleStmt => arm!(RuleStmt, out_rule_stmt),
            NodeTag::T_ListenStmt => arm!(ListenStmt, out_listen_stmt),
            NodeTag::T_UnlistenStmt => arm!(UnlistenStmt, out_unlisten_stmt),
            NodeTag::T_CompositeTypeStmt => arm!(CompositeTypeStmt, out_composite_type_stmt),
            NodeTag::T_CreateEnumStmt => arm!(CreateEnumStmt, out_create_enum_stmt),
            NodeTag::T_CreateRangeStmt => arm!(CreateRangeStmt, out_create_range_stmt),
            NodeTag::T_AlterEnumStmt => arm!(AlterEnumStmt, out_alter_enum_stmt),
            NodeTag::T_ViewStmt => arm!(ViewStmt, out_view_stmt),
            NodeTag::T_LoadStmt => arm!(LoadStmt, out_load_stmt),
            NodeTag::T_CreatedbStmt => arm!(CreatedbStmt, out_createdb_stmt),
            NodeTag::T_AlterDatabaseStmt => arm!(AlterDatabaseStmt, out_alter_database_stmt),
            NodeTag::T_AlterDatabaseRefreshCollStmt => arm!(AlterDatabaseRefreshCollStmt, out_alter_database_refresh_coll_stmt),
            NodeTag::T_AlterDatabaseSetStmt => arm!(AlterDatabaseSetStmt, out_alter_database_set_stmt),
            NodeTag::T_DropdbStmt => arm!(DropdbStmt, out_dropdb_stmt),
            NodeTag::T_AlterSystemStmt => arm!(AlterSystemStmt, out_alter_system_stmt),
            NodeTag::T_ClusterStmt => arm!(ClusterStmt, out_cluster_stmt),
            NodeTag::T_VacuumStmt => arm!(VacuumStmt, out_vacuum_stmt),
            NodeTag::T_VacuumRelation => arm!(VacuumRelation, out_vacuum_relation),
            NodeTag::T_ExplainStmt => arm!(ExplainStmt, out_explain_stmt),
            NodeTag::T_CreateTableAsStmt => arm!(CreateTableAsStmt, out_create_table_as_stmt),
            NodeTag::T_RefreshMatViewStmt => arm!(RefreshMatViewStmt, out_refresh_mat_view_stmt),
            NodeTag::T_CheckPointStmt => arm!(CheckPointStmt, out_check_point_stmt),
            NodeTag::T_DiscardStmt => arm!(DiscardStmt, out_discard_stmt),
            NodeTag::T_LockStmt => arm!(LockStmt, out_lock_stmt),
            NodeTag::T_ConstraintsSetStmt => arm!(ConstraintsSetStmt, out_constraints_set_stmt),
            NodeTag::T_ReindexStmt => arm!(ReindexStmt, out_reindex_stmt),
            NodeTag::T_CreateConversionStmt => arm!(CreateConversionStmt, out_create_conversion_stmt),
            NodeTag::T_CreateCastStmt => arm!(CreateCastStmt, out_create_cast_stmt),
            NodeTag::T_CreateTransformStmt => arm!(CreateTransformStmt, out_create_transform_stmt),
            NodeTag::T_DropOwnedStmt => arm!(DropOwnedStmt, out_drop_owned_stmt),
            NodeTag::T_ReassignOwnedStmt => arm!(ReassignOwnedStmt, out_reassign_owned_stmt),
            NodeTag::T_AlterTSDictionaryStmt => arm!(AlterTSDictionaryStmt, out_alter_ts_dictionary_stmt),
            NodeTag::T_AlterTSConfigurationStmt => arm!(AlterTSConfigurationStmt, out_alter_ts_configuration_stmt),
            NodeTag::T_PublicationTable => arm!(PublicationTable, out_publication_table),
            NodeTag::T_PublicationObjSpec => arm!(PublicationObjSpec, out_publication_obj_spec),
            NodeTag::T_CreatePublicationStmt => arm!(CreatePublicationStmt, out_create_publication_stmt),
            NodeTag::T_AlterPublicationStmt => arm!(AlterPublicationStmt, out_alter_publication_stmt),
            NodeTag::T_CreateSubscriptionStmt => arm!(CreateSubscriptionStmt, out_create_subscription_stmt),
            NodeTag::T_AlterSubscriptionStmt => arm!(AlterSubscriptionStmt, out_alter_subscription_stmt),
            NodeTag::T_DropSubscriptionStmt => arm!(DropSubscriptionStmt, out_drop_subscription_stmt),
            _ => {
                handled = false;
                Ok(())
            }
        }
    })();
    if handled {
        Some(r)
    } else {
        None
    }
}

pub(super) fn out_into_clause(out: &mut PgString<'_>, node: &IntoClause<'_>) -> PgResult<()> {
    w!(out, "{{INTOCLAUSE");
    w!(out, " :rel ");
    out_opt_node(out, node.rel)?;
    w!(out, " :colNames ");
    out_list(out, &node.colNames)?;
    w!(out, " :accessMethod ");
    out_str(out, node.accessMethod);
    w!(out, " :options ");
    out_list(out, &node.options)?;
    w!(out, " :onCommit {}", node.onCommit as i32);
    w!(out, " :tableSpaceName ");
    out_str(out, node.tableSpaceName);
    w!(out, " :viewQuery ");
    out_opt_node(out, node.viewQuery)?;
    w!(out, " :skipData ");
    out_bool(out, node.skipData);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_role_spec(out: &mut PgString<'_>, node: &RoleSpec<'_>) -> PgResult<()> {
    w!(out, "{{ROLESPEC");
    w!(out, " :roletype {}", node.roletype as i32);
    w!(out, " :rolename ");
    out_str(out, node.rolename);
    w!(out, " :location {}", loc(node.location));
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_range_table_func(out: &mut PgString<'_>, node: &RangeTableFunc<'_>) -> PgResult<()> {
    w!(out, "{{RANGETABLEFUNC");
    w!(out, " :lateral ");
    out_bool(out, node.lateral);
    w!(out, " :docexpr ");
    out_opt_node(out, node.docexpr)?;
    w!(out, " :rowexpr ");
    out_opt_node(out, node.rowexpr)?;
    w!(out, " :namespaces ");
    out_list(out, &node.namespaces)?;
    w!(out, " :columns ");
    out_list(out, &node.columns)?;
    w!(out, " :alias ");
    out_opt_alias(out, node.alias)?;
    w!(out, " :location {}", loc(node.location));
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_range_table_func_col(out: &mut PgString<'_>, node: &RangeTableFuncCol<'_>) -> PgResult<()> {
    w!(out, "{{RANGETABLEFUNCCOL");
    w!(out, " :colname ");
    out_str(out, node.colname);
    w!(out, " :typeName ");
    out_opt_node(out, node.typeName)?;
    w!(out, " :for_ordinality ");
    out_bool(out, node.for_ordinality);
    w!(out, " :is_not_null ");
    out_bool(out, node.is_not_null);
    w!(out, " :colexpr ");
    out_opt_node(out, node.colexpr)?;
    w!(out, " :coldefexpr ");
    out_opt_node(out, node.coldefexpr)?;
    w!(out, " :location {}", loc(node.location));
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_range_table_sample(out: &mut PgString<'_>, node: &RangeTableSample<'_>) -> PgResult<()> {
    w!(out, "{{RANGETABLESAMPLE");
    w!(out, " :relation ");
    out_opt_node(out, node.relation)?;
    w!(out, " :method ");
    out_list(out, &node.method)?;
    w!(out, " :args ");
    out_list(out, &node.args)?;
    w!(out, " :repeatable ");
    out_opt_node(out, node.repeatable)?;
    w!(out, " :location {}", loc(node.location));
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_table_like_clause(out: &mut PgString<'_>, node: &TableLikeClause<'_>) -> PgResult<()> {
    w!(out, "{{TABLELIKECLAUSE");
    w!(out, " :relation ");
    out_opt_range_var(out, node.relation)?;
    w!(out, " :options {}", node.options);
    w!(out, " :relationOid {}", node.relationOid);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_index_elem(out: &mut PgString<'_>, node: &IndexElem<'_>) -> PgResult<()> {
    w!(out, "{{INDEXELEM");
    w!(out, " :name ");
    out_str(out, node.name);
    w!(out, " :expr ");
    out_opt_node(out, node.expr)?;
    w!(out, " :indexcolname ");
    out_str(out, node.indexcolname);
    w!(out, " :collation ");
    out_list(out, &node.collation)?;
    w!(out, " :opclass ");
    out_list(out, &node.opclass)?;
    w!(out, " :opclassopts ");
    out_list(out, &node.opclassopts)?;
    w!(out, " :ordering {}", node.ordering as i32);
    w!(out, " :nulls_ordering {}", node.nulls_ordering as i32);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_def_elem(out: &mut PgString<'_>, node: &DefElem<'_>) -> PgResult<()> {
    w!(out, "{{DEFELEM");
    w!(out, " :defnamespace ");
    out_str(out, node.defnamespace);
    w!(out, " :defname ");
    out_str(out, node.defname);
    w!(out, " :arg ");
    out_opt_node(out, node.arg)?;
    w!(out, " :defaction {}", node.defaction as i32);
    w!(out, " :location {}", loc(node.location));
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_xml_serialize(out: &mut PgString<'_>, node: &XmlSerialize<'_>) -> PgResult<()> {
    w!(out, "{{XMLSERIALIZE");
    w!(out, " :xmloption {}", node.xmloption as i32);
    w!(out, " :expr ");
    out_opt_node(out, node.expr)?;
    w!(out, " :typeName ");
    out_opt_node(out, node.typeName)?;
    w!(out, " :indent ");
    out_bool(out, node.indent);
    w!(out, " :location {}", loc(node.location));
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_partition_elem(out: &mut PgString<'_>, node: &PartitionElem<'_>) -> PgResult<()> {
    w!(out, "{{PARTITIONELEM");
    w!(out, " :name ");
    out_str(out, node.name);
    w!(out, " :expr ");
    out_opt_node(out, node.expr)?;
    w!(out, " :collation ");
    out_list(out, &node.collation)?;
    w!(out, " :opclass ");
    out_list(out, &node.opclass)?;
    w!(out, " :location {}", loc(node.location));
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_partition_spec(out: &mut PgString<'_>, node: &PartitionSpec<'_>) -> PgResult<()> {
    w!(out, "{{PARTITIONSPEC");
    w!(out, " :strategy {}", node.strategy as i32);
    w!(out, " :partParams ");
    out_list(out, &node.partParams)?;
    w!(out, " :location {}", loc(node.location));
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_partition_cmd(out: &mut PgString<'_>, node: &PartitionCmd<'_>) -> PgResult<()> {
    w!(out, "{{PARTITIONCMD");
    w!(out, " :name ");
    out_opt_range_var(out, node.name)?;
    w!(out, " :bound ");
    out_opt_node(out, node.bound)?;
    w!(out, " :concurrent ");
    out_bool(out, node.concurrent);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_merge_when_clause(out: &mut PgString<'_>, node: &MergeWhenClause<'_>) -> PgResult<()> {
    w!(out, "{{MERGEWHENCLAUSE");
    w!(out, " :matchKind {}", node.matchKind as i32);
    w!(out, " :commandType {}", node.commandType as i32);
    w!(out, " :override {}", node.r#override as i32);
    w!(out, " :condition ");
    out_opt_node(out, node.condition)?;
    w!(out, " :targetList ");
    out_list(out, &node.targetList)?;
    w!(out, " :values ");
    out_list(out, &node.values)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_trigger_transition(out: &mut PgString<'_>, node: &TriggerTransition<'_>) -> PgResult<()> {
    w!(out, "{{TRIGGERTRANSITION");
    w!(out, " :name ");
    out_str(out, node.name);
    w!(out, " :isNew ");
    out_bool(out, node.isNew);
    w!(out, " :isTable ");
    out_bool(out, node.isTable);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_json_output(out: &mut PgString<'_>, node: &JsonOutput<'_>) -> PgResult<()> {
    w!(out, "{{JSONOUTPUT");
    w!(out, " :typeName ");
    out_opt_node(out, node.typeName)?;
    w!(out, " :returning ");
    out_opt_json_returning(out, node.returning);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_json_argument(out: &mut PgString<'_>, node: &JsonArgument<'_>) -> PgResult<()> {
    w!(out, "{{JSONARGUMENT");
    w!(out, " :val ");
    out_opt_node(out, node.val)?;
    w!(out, " :name ");
    out_str(out, node.name);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_json_func_expr(out: &mut PgString<'_>, node: &JsonFuncExpr<'_>) -> PgResult<()> {
    w!(out, "{{JSONFUNCEXPR");
    w!(out, " :op {}", node.op as i32);
    w!(out, " :column_name ");
    out_str(out, node.column_name);
    w!(out, " :context_item ");
    out_opt_node(out, node.context_item)?;
    w!(out, " :pathspec ");
    out_opt_node(out, node.pathspec)?;
    w!(out, " :passing ");
    out_list(out, &node.passing)?;
    w!(out, " :output ");
    out_opt_node(out, node.output)?;
    w!(out, " :on_empty ");
    out_opt_node(out, node.on_empty)?;
    w!(out, " :on_error ");
    out_opt_node(out, node.on_error)?;
    w!(out, " :wrapper {}", node.wrapper as i32);
    w!(out, " :quotes {}", node.quotes as i32);
    w!(out, " :location {}", loc(node.location));
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_json_table_path_spec(out: &mut PgString<'_>, node: &JsonTablePathSpec<'_>) -> PgResult<()> {
    w!(out, "{{JSONTABLEPATHSPEC");
    w!(out, " :string ");
    out_opt_node(out, node.string)?;
    w!(out, " :name ");
    out_str(out, node.name);
    w!(out, " :name_location {}", loc(node.name_location));
    w!(out, " :location {}", loc(node.location));
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_json_table(out: &mut PgString<'_>, node: &JsonTable<'_>) -> PgResult<()> {
    w!(out, "{{JSONTABLE");
    w!(out, " :context_item ");
    out_opt_node(out, node.context_item)?;
    w!(out, " :pathspec ");
    out_opt_node(out, node.pathspec)?;
    w!(out, " :passing ");
    out_list(out, &node.passing)?;
    w!(out, " :columns ");
    out_list(out, &node.columns)?;
    w!(out, " :on_error ");
    out_opt_node(out, node.on_error)?;
    w!(out, " :alias ");
    out_opt_alias(out, node.alias)?;
    w!(out, " :lateral ");
    out_bool(out, node.lateral);
    w!(out, " :location {}", loc(node.location));
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_json_table_column(out: &mut PgString<'_>, node: &JsonTableColumn<'_>) -> PgResult<()> {
    w!(out, "{{JSONTABLECOLUMN");
    w!(out, " :coltype {}", node.coltype as i32);
    w!(out, " :name ");
    out_str(out, node.name);
    w!(out, " :typeName ");
    out_opt_node(out, node.typeName)?;
    w!(out, " :pathspec ");
    out_opt_node(out, node.pathspec)?;
    w!(out, " :format ");
    out_opt_json_format(out, node.format);
    w!(out, " :wrapper {}", node.wrapper as i32);
    w!(out, " :quotes {}", node.quotes as i32);
    w!(out, " :columns ");
    out_list(out, &node.columns)?;
    w!(out, " :on_empty ");
    out_opt_node(out, node.on_empty)?;
    w!(out, " :on_error ");
    out_opt_node(out, node.on_error)?;
    w!(out, " :location {}", loc(node.location));
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_json_key_value(out: &mut PgString<'_>, node: &JsonKeyValue<'_>) -> PgResult<()> {
    w!(out, "{{JSONKEYVALUE");
    w!(out, " :key ");
    out_opt_node(out, node.key)?;
    w!(out, " :value ");
    out_opt_node(out, node.value)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_json_parse_expr(out: &mut PgString<'_>, node: &JsonParseExpr<'_>) -> PgResult<()> {
    w!(out, "{{JSONPARSEEXPR");
    w!(out, " :expr ");
    out_opt_node(out, node.expr)?;
    w!(out, " :output ");
    out_opt_node(out, node.output)?;
    w!(out, " :unique_keys ");
    out_bool(out, node.unique_keys);
    w!(out, " :location {}", loc(node.location));
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_json_scalar_expr(out: &mut PgString<'_>, node: &JsonScalarExpr<'_>) -> PgResult<()> {
    w!(out, "{{JSONSCALAREXPR");
    w!(out, " :expr ");
    out_opt_node(out, node.expr)?;
    w!(out, " :output ");
    out_opt_node(out, node.output)?;
    w!(out, " :location {}", loc(node.location));
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_json_serialize_expr(out: &mut PgString<'_>, node: &JsonSerializeExpr<'_>) -> PgResult<()> {
    w!(out, "{{JSONSERIALIZEEXPR");
    w!(out, " :expr ");
    out_opt_node(out, node.expr)?;
    w!(out, " :output ");
    out_opt_node(out, node.output)?;
    w!(out, " :location {}", loc(node.location));
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_json_object_constructor(out: &mut PgString<'_>, node: &JsonObjectConstructor<'_>) -> PgResult<()> {
    w!(out, "{{JSONOBJECTCONSTRUCTOR");
    w!(out, " :exprs ");
    out_list(out, &node.exprs)?;
    w!(out, " :output ");
    out_opt_node(out, node.output)?;
    w!(out, " :absent_on_null ");
    out_bool(out, node.absent_on_null);
    w!(out, " :unique ");
    out_bool(out, node.unique);
    w!(out, " :location {}", loc(node.location));
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_json_array_constructor(out: &mut PgString<'_>, node: &JsonArrayConstructor<'_>) -> PgResult<()> {
    w!(out, "{{JSONARRAYCONSTRUCTOR");
    w!(out, " :exprs ");
    out_list(out, &node.exprs)?;
    w!(out, " :output ");
    out_opt_node(out, node.output)?;
    w!(out, " :absent_on_null ");
    out_bool(out, node.absent_on_null);
    w!(out, " :location {}", loc(node.location));
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_json_array_query_constructor(out: &mut PgString<'_>, node: &JsonArrayQueryConstructor<'_>) -> PgResult<()> {
    w!(out, "{{JSONARRAYQUERYCONSTRUCTOR");
    w!(out, " :query ");
    out_opt_node(out, node.query)?;
    w!(out, " :output ");
    out_opt_node(out, node.output)?;
    w!(out, " :format ");
    out_opt_json_format(out, node.format);
    w!(out, " :absent_on_null ");
    out_bool(out, node.absent_on_null);
    w!(out, " :location {}", loc(node.location));
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_json_agg_constructor(out: &mut PgString<'_>, node: &JsonAggConstructor<'_>) -> PgResult<()> {
    w!(out, "{{JSONAGGCONSTRUCTOR");
    w!(out, " :output ");
    out_opt_node(out, node.output)?;
    w!(out, " :agg_filter ");
    out_opt_node(out, node.agg_filter)?;
    w!(out, " :agg_order ");
    out_list(out, &node.agg_order)?;
    w!(out, " :over ");
    out_opt_node(out, node.over)?;
    w!(out, " :location {}", loc(node.location));
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_json_object_agg(out: &mut PgString<'_>, node: &JsonObjectAgg<'_>) -> PgResult<()> {
    w!(out, "{{JSONOBJECTAGG");
    w!(out, " :constructor ");
    out_opt_node(out, node.constructor)?;
    w!(out, " :arg ");
    out_opt_node(out, node.arg)?;
    w!(out, " :absent_on_null ");
    out_bool(out, node.absent_on_null);
    w!(out, " :unique ");
    out_bool(out, node.unique);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_json_array_agg(out: &mut PgString<'_>, node: &JsonArrayAgg<'_>) -> PgResult<()> {
    w!(out, "{{JSONARRAYAGG");
    w!(out, " :constructor ");
    out_opt_node(out, node.constructor)?;
    w!(out, " :arg ");
    out_opt_node(out, node.arg)?;
    w!(out, " :absent_on_null ");
    out_bool(out, node.absent_on_null);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_merge_stmt(out: &mut PgString<'_>, node: &MergeStmt<'_>) -> PgResult<()> {
    w!(out, "{{MERGESTMT");
    w!(out, " :relation ");
    out_opt_node(out, node.relation)?;
    w!(out, " :sourceRelation ");
    out_opt_node(out, node.sourceRelation)?;
    w!(out, " :joinCondition ");
    out_opt_node(out, node.joinCondition)?;
    w!(out, " :mergeWhenClauses ");
    out_list(out, &node.mergeWhenClauses)?;
    w!(out, " :returningClause ");
    out_opt_node(out, node.returningClause)?;
    w!(out, " :withClause ");
    out_opt_node(out, node.withClause)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_return_stmt(out: &mut PgString<'_>, node: &ReturnStmt<'_>) -> PgResult<()> {
    w!(out, "{{RETURNSTMT");
    w!(out, " :returnval ");
    out_opt_node(out, node.returnval)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_pl_assign_stmt(out: &mut PgString<'_>, node: &PLAssignStmt<'_>) -> PgResult<()> {
    w!(out, "{{PLASSIGNSTMT");
    w!(out, " :name ");
    out_token(out, node.name);
    w!(out, " :indirection ");
    out_list(out, &node.indirection)?;
    w!(out, " :nnames {}", node.nnames);
    w!(out, " :val ");
    out_opt_node(out, node.val)?;
    w!(out, " :location {}", loc(node.location));
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_create_schema_stmt(out: &mut PgString<'_>, node: &CreateSchemaStmt<'_>) -> PgResult<()> {
    w!(out, "{{CREATESCHEMASTMT");
    w!(out, " :schemaname ");
    out_str(out, node.schemaname);
    w!(out, " :authrole ");
    out_opt_node(out, node.authrole)?;
    w!(out, " :schemaElts ");
    out_list(out, &node.schemaElts)?;
    w!(out, " :if_not_exists ");
    out_bool(out, node.if_not_exists);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_alter_table_stmt(out: &mut PgString<'_>, node: &AlterTableStmt<'_>) -> PgResult<()> {
    w!(out, "{{ALTERTABLESTMT");
    w!(out, " :relation ");
    out_opt_range_var(out, node.relation)?;
    w!(out, " :cmds ");
    out_list(out, &node.cmds)?;
    w!(out, " :objtype {}", node.objtype as i32);
    w!(out, " :missing_ok ");
    out_bool(out, node.missing_ok);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_alter_table_cmd(out: &mut PgString<'_>, node: &AlterTableCmd<'_>) -> PgResult<()> {
    w!(out, "{{ALTERTABLECMD");
    w!(out, " :subtype {}", node.subtype as i32);
    w!(out, " :name ");
    out_str(out, node.name);
    w!(out, " :num {}", node.num);
    w!(out, " :newowner ");
    out_opt_node(out, node.newowner)?;
    w!(out, " :def ");
    out_opt_node(out, node.def)?;
    w!(out, " :behavior {}", node.behavior as i32);
    w!(out, " :missing_ok ");
    out_bool(out, node.missing_ok);
    w!(out, " :recurse ");
    out_bool(out, node.recurse);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_at_alter_constraint(out: &mut PgString<'_>, node: &ATAlterConstraint<'_>) -> PgResult<()> {
    w!(out, "{{ATALTERCONSTRAINT");
    w!(out, " :conname ");
    out_str(out, node.conname);
    w!(out, " :alterEnforceability ");
    out_bool(out, node.alterEnforceability);
    w!(out, " :is_enforced ");
    out_bool(out, node.is_enforced);
    w!(out, " :alterDeferrability ");
    out_bool(out, node.alterDeferrability);
    w!(out, " :deferrable ");
    out_bool(out, node.deferrable);
    w!(out, " :initdeferred ");
    out_bool(out, node.initdeferred);
    w!(out, " :alterInheritability ");
    out_bool(out, node.alterInheritability);
    w!(out, " :noinherit ");
    out_bool(out, node.noinherit);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_replica_identity_stmt(out: &mut PgString<'_>, node: &ReplicaIdentityStmt<'_>) -> PgResult<()> {
    w!(out, "{{REPLICAIDENTITYSTMT");
    w!(out, " :identity_type ");
    out_char(out, node.identity_type as u8)?;
    w!(out, " :name ");
    out_str(out, node.name);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_alter_collation_stmt(out: &mut PgString<'_>, node: &AlterCollationStmt<'_>) -> PgResult<()> {
    w!(out, "{{ALTERCOLLATIONSTMT");
    w!(out, " :collname ");
    out_list(out, &node.collname)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_alter_domain_stmt(out: &mut PgString<'_>, node: &AlterDomainStmt<'_>) -> PgResult<()> {
    w!(out, "{{ALTERDOMAINSTMT");
    w!(out, " :subtype ");
    out_char(out, node.subtype as u8)?;
    w!(out, " :typeName ");
    out_list(out, &node.typeName)?;
    w!(out, " :name ");
    out_str(out, node.name);
    w!(out, " :def ");
    out_opt_node(out, node.def)?;
    w!(out, " :behavior {}", node.behavior as i32);
    w!(out, " :missing_ok ");
    out_bool(out, node.missing_ok);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_grant_stmt(out: &mut PgString<'_>, node: &GrantStmt<'_>) -> PgResult<()> {
    w!(out, "{{GRANTSTMT");
    w!(out, " :is_grant ");
    out_bool(out, node.is_grant);
    w!(out, " :targtype {}", node.targtype as i32);
    w!(out, " :objtype {}", node.objtype as i32);
    w!(out, " :objects ");
    out_list(out, &node.objects)?;
    w!(out, " :privileges ");
    out_list(out, &node.privileges)?;
    w!(out, " :grantees ");
    out_list(out, &node.grantees)?;
    w!(out, " :grant_option ");
    out_bool(out, node.grant_option);
    w!(out, " :grantor ");
    match node.grantor { Some(r) => out_role_spec(out, r)?, None => w!(out, "<>") };
    w!(out, " :behavior {}", node.behavior as i32);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_access_priv(out: &mut PgString<'_>, node: &AccessPriv<'_>) -> PgResult<()> {
    w!(out, "{{ACCESSPRIV");
    w!(out, " :priv_name ");
    out_str(out, node.priv_name);
    w!(out, " :cols ");
    out_list(out, &node.cols)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_grant_role_stmt(out: &mut PgString<'_>, node: &GrantRoleStmt<'_>) -> PgResult<()> {
    w!(out, "{{GRANTROLESTMT");
    w!(out, " :granted_roles ");
    out_list(out, &node.granted_roles)?;
    w!(out, " :grantee_roles ");
    out_list(out, &node.grantee_roles)?;
    w!(out, " :is_grant ");
    out_bool(out, node.is_grant);
    w!(out, " :opt ");
    out_list(out, &node.opt)?;
    w!(out, " :grantor ");
    match node.grantor { Some(r) => out_role_spec(out, r)?, None => w!(out, "<>") };
    w!(out, " :behavior {}", node.behavior as i32);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_alter_default_privileges_stmt(out: &mut PgString<'_>, node: &AlterDefaultPrivilegesStmt<'_>) -> PgResult<()> {
    w!(out, "{{ALTERDEFAULTPRIVILEGESSTMT");
    w!(out, " :options ");
    out_list(out, &node.options)?;
    w!(out, " :action ");
    match node.action { Some(g) => out_grant_stmt(out, g)?, None => w!(out, "<>") };
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_copy_stmt(out: &mut PgString<'_>, node: &CopyStmt<'_>) -> PgResult<()> {
    w!(out, "{{COPYSTMT");
    w!(out, " :relation ");
    out_opt_node(out, node.relation)?;
    w!(out, " :query ");
    out_opt_node(out, node.query)?;
    w!(out, " :attlist ");
    out_list(out, &node.attlist)?;
    w!(out, " :is_from ");
    out_bool(out, node.is_from);
    w!(out, " :is_program ");
    out_bool(out, node.is_program);
    w!(out, " :filename ");
    out_str(out, node.filename);
    w!(out, " :options ");
    out_list(out, &node.options)?;
    w!(out, " :whereClause ");
    out_opt_node(out, node.whereClause)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_constraint(out: &mut PgString<'_>, node: &Constraint<'_>) -> PgResult<()> {
    w!(out, "{{CONSTRAINT");
    w!(out, " :contype {}", node.contype as i32);
    w!(out, " :conname ");
    out_str(out, node.conname);
    w!(out, " :deferrable ");
    out_bool(out, node.deferrable);
    w!(out, " :initdeferred ");
    out_bool(out, node.initdeferred);
    w!(out, " :is_enforced ");
    out_bool(out, node.is_enforced);
    w!(out, " :skip_validation ");
    out_bool(out, node.skip_validation);
    w!(out, " :initially_valid ");
    out_bool(out, node.initially_valid);
    w!(out, " :is_no_inherit ");
    out_bool(out, node.is_no_inherit);
    w!(out, " :raw_expr ");
    out_opt_node(out, node.raw_expr)?;
    w!(out, " :cooked_expr ");
    out_str(out, node.cooked_expr);
    w!(out, " :generated_when ");
    out_char(out, node.generated_when as u8)?;
    w!(out, " :generated_kind ");
    out_char(out, node.generated_kind as u8)?;
    w!(out, " :nulls_not_distinct ");
    out_bool(out, node.nulls_not_distinct);
    w!(out, " :keys ");
    out_list(out, &node.keys)?;
    w!(out, " :without_overlaps ");
    out_bool(out, node.without_overlaps);
    w!(out, " :including ");
    out_list(out, &node.including)?;
    w!(out, " :exclusions ");
    out_list(out, &node.exclusions)?;
    w!(out, " :options ");
    out_list(out, &node.options)?;
    w!(out, " :indexname ");
    out_str(out, node.indexname);
    w!(out, " :indexspace ");
    out_str(out, node.indexspace);
    w!(out, " :reset_default_tblspc ");
    out_bool(out, node.reset_default_tblspc);
    w!(out, " :access_method ");
    out_str(out, node.access_method);
    w!(out, " :where_clause ");
    out_opt_node(out, node.where_clause)?;
    w!(out, " :pktable ");
    out_opt_range_var(out, node.pktable)?;
    w!(out, " :fk_attrs ");
    out_list(out, &node.fk_attrs)?;
    w!(out, " :pk_attrs ");
    out_list(out, &node.pk_attrs)?;
    w!(out, " :fk_with_period ");
    out_bool(out, node.fk_with_period);
    w!(out, " :pk_with_period ");
    out_bool(out, node.pk_with_period);
    w!(out, " :fk_matchtype ");
    out_char(out, node.fk_matchtype as u8)?;
    w!(out, " :fk_upd_action ");
    out_char(out, node.fk_upd_action as u8)?;
    w!(out, " :fk_del_action ");
    out_char(out, node.fk_del_action as u8)?;
    w!(out, " :fk_del_set_cols ");
    out_list(out, &node.fk_del_set_cols)?;
    w!(out, " :old_conpfeqop ");
    out_oid_list(out, &node.old_conpfeqop);
    w!(out, " :old_pktable_oid {}", node.old_pktable_oid);
    w!(out, " :location {}", loc(node.location));
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_create_table_space_stmt(out: &mut PgString<'_>, node: &CreateTableSpaceStmt<'_>) -> PgResult<()> {
    w!(out, "{{CREATETABLESPACESTMT");
    w!(out, " :tablespacename ");
    out_str(out, node.tablespacename);
    w!(out, " :owner ");
    match node.owner { Some(r) => out_role_spec(out, r)?, None => w!(out, "<>") };
    w!(out, " :location ");
    out_str(out, node.location);
    w!(out, " :options ");
    out_list(out, &node.options)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_drop_table_space_stmt(out: &mut PgString<'_>, node: &DropTableSpaceStmt<'_>) -> PgResult<()> {
    w!(out, "{{DROPTABLESPACESTMT");
    w!(out, " :tablespacename ");
    out_str(out, node.tablespacename);
    w!(out, " :missing_ok ");
    out_bool(out, node.missing_ok);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_alter_table_space_options_stmt(out: &mut PgString<'_>, node: &AlterTableSpaceOptionsStmt<'_>) -> PgResult<()> {
    w!(out, "{{ALTERTABLESPACEOPTIONSSTMT");
    w!(out, " :tablespacename ");
    out_str(out, node.tablespacename);
    w!(out, " :options ");
    out_list(out, &node.options)?;
    w!(out, " :isReset ");
    out_bool(out, node.isReset);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_alter_table_move_all_stmt(out: &mut PgString<'_>, node: &AlterTableMoveAllStmt<'_>) -> PgResult<()> {
    w!(out, "{{ALTERTABLEMOVEALLSTMT");
    w!(out, " :orig_tablespacename ");
    out_str(out, node.orig_tablespacename);
    w!(out, " :objtype {}", node.objtype as i32);
    w!(out, " :roles ");
    out_list(out, &node.roles)?;
    w!(out, " :new_tablespacename ");
    out_str(out, node.new_tablespacename);
    w!(out, " :nowait ");
    out_bool(out, node.nowait);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_create_extension_stmt(out: &mut PgString<'_>, node: &CreateExtensionStmt<'_>) -> PgResult<()> {
    w!(out, "{{CREATEEXTENSIONSTMT");
    w!(out, " :extname ");
    out_str(out, node.extname);
    w!(out, " :if_not_exists ");
    out_bool(out, node.if_not_exists);
    w!(out, " :options ");
    out_list(out, &node.options)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_alter_extension_stmt(out: &mut PgString<'_>, node: &AlterExtensionStmt<'_>) -> PgResult<()> {
    w!(out, "{{ALTEREXTENSIONSTMT");
    w!(out, " :extname ");
    out_str(out, node.extname);
    w!(out, " :options ");
    out_list(out, &node.options)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_alter_extension_contents_stmt(out: &mut PgString<'_>, node: &AlterExtensionContentsStmt<'_>) -> PgResult<()> {
    w!(out, "{{ALTEREXTENSIONCONTENTSSTMT");
    w!(out, " :extname ");
    out_str(out, node.extname);
    w!(out, " :action {}", node.action);
    w!(out, " :objtype {}", node.objtype as i32);
    w!(out, " :object ");
    out_opt_node(out, node.object)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_create_fdw_stmt(out: &mut PgString<'_>, node: &CreateFdwStmt<'_>) -> PgResult<()> {
    w!(out, "{{CREATEFDWSTMT");
    w!(out, " :fdwname ");
    out_str(out, node.fdwname);
    w!(out, " :func_options ");
    out_list(out, &node.func_options)?;
    w!(out, " :options ");
    out_list(out, &node.options)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_alter_fdw_stmt(out: &mut PgString<'_>, node: &AlterFdwStmt<'_>) -> PgResult<()> {
    w!(out, "{{ALTERFDWSTMT");
    w!(out, " :fdwname ");
    out_str(out, node.fdwname);
    w!(out, " :func_options ");
    out_list(out, &node.func_options)?;
    w!(out, " :options ");
    out_list(out, &node.options)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_create_foreign_server_stmt(out: &mut PgString<'_>, node: &CreateForeignServerStmt<'_>) -> PgResult<()> {
    w!(out, "{{CREATEFOREIGNSERVERSTMT");
    w!(out, " :servername ");
    out_str(out, node.servername);
    w!(out, " :servertype ");
    out_str(out, node.servertype);
    w!(out, " :version ");
    out_str(out, node.version);
    w!(out, " :fdwname ");
    out_str(out, node.fdwname);
    w!(out, " :if_not_exists ");
    out_bool(out, node.if_not_exists);
    w!(out, " :options ");
    out_list(out, &node.options)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_alter_foreign_server_stmt(out: &mut PgString<'_>, node: &AlterForeignServerStmt<'_>) -> PgResult<()> {
    w!(out, "{{ALTERFOREIGNSERVERSTMT");
    w!(out, " :servername ");
    out_str(out, node.servername);
    w!(out, " :version ");
    out_str(out, node.version);
    w!(out, " :options ");
    out_list(out, &node.options)?;
    w!(out, " :has_version ");
    out_bool(out, node.has_version);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_create_foreign_table_stmt(out: &mut PgString<'_>, node: &CreateForeignTableStmt<'_>) -> PgResult<()> {
    w!(out, "{{CREATEFOREIGNTABLESTMT");
    w!(out, " :base.relation ");
    out_opt_range_var(out, node.base.relation)?;
    w!(out, " :base.tableElts ");
    out_list(out, &node.base.tableElts)?;
    w!(out, " :base.inhRelations ");
    out_list(out, &node.base.inhRelations)?;
    w!(out, " :base.partbound ");
    out_opt_node(out, node.base.partbound)?;
    w!(out, " :base.partspec ");
    out_opt_node(out, node.base.partspec)?;
    w!(out, " :base.ofTypename ");
    out_opt_node(out, node.base.ofTypename)?;
    w!(out, " :base.constraints ");
    out_list(out, &node.base.constraints)?;
    w!(out, " :base.nnconstraints ");
    out_list(out, &node.base.nnconstraints)?;
    w!(out, " :base.options ");
    out_list(out, &node.base.options)?;
    w!(out, " :base.oncommit {}", node.base.oncommit as u32);
    w!(out, " :base.tablespacename ");
    out_str(out, node.base.tablespacename);
    w!(out, " :base.accessMethod ");
    out_str(out, node.base.accessMethod);
    w!(out, " :base.if_not_exists ");
    out_bool(out, node.base.if_not_exists);
    w!(out, " :servername ");
    out_str(out, node.servername);
    w!(out, " :options ");
    out_list(out, &node.options)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_create_user_mapping_stmt(out: &mut PgString<'_>, node: &CreateUserMappingStmt<'_>) -> PgResult<()> {
    w!(out, "{{CREATEUSERMAPPINGSTMT");
    w!(out, " :user ");
    match node.user { Some(r) => out_role_spec(out, r)?, None => w!(out, "<>") };
    w!(out, " :servername ");
    out_str(out, node.servername);
    w!(out, " :if_not_exists ");
    out_bool(out, node.if_not_exists);
    w!(out, " :options ");
    out_list(out, &node.options)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_alter_user_mapping_stmt(out: &mut PgString<'_>, node: &AlterUserMappingStmt<'_>) -> PgResult<()> {
    w!(out, "{{ALTERUSERMAPPINGSTMT");
    w!(out, " :user ");
    match node.user { Some(r) => out_role_spec(out, r)?, None => w!(out, "<>") };
    w!(out, " :servername ");
    out_str(out, node.servername);
    w!(out, " :options ");
    out_list(out, &node.options)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_drop_user_mapping_stmt(out: &mut PgString<'_>, node: &DropUserMappingStmt<'_>) -> PgResult<()> {
    w!(out, "{{DROPUSERMAPPINGSTMT");
    w!(out, " :user ");
    match node.user { Some(r) => out_role_spec(out, r)?, None => w!(out, "<>") };
    w!(out, " :servername ");
    out_str(out, node.servername);
    w!(out, " :missing_ok ");
    out_bool(out, node.missing_ok);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_import_foreign_schema_stmt(out: &mut PgString<'_>, node: &ImportForeignSchemaStmt<'_>) -> PgResult<()> {
    w!(out, "{{IMPORTFOREIGNSCHEMASTMT");
    w!(out, " :server_name ");
    out_str(out, node.server_name);
    w!(out, " :remote_schema ");
    out_str(out, node.remote_schema);
    w!(out, " :local_schema ");
    out_str(out, node.local_schema);
    w!(out, " :list_type {}", node.list_type as i32);
    w!(out, " :table_list ");
    out_list(out, &node.table_list)?;
    w!(out, " :options ");
    out_list(out, &node.options)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_create_policy_stmt(out: &mut PgString<'_>, node: &CreatePolicyStmt<'_>) -> PgResult<()> {
    w!(out, "{{CREATEPOLICYSTMT");
    w!(out, " :policy_name ");
    out_str(out, node.policy_name);
    w!(out, " :table ");
    out_opt_range_var(out, node.table)?;
    w!(out, " :cmd_name ");
    out_str(out, node.cmd_name);
    w!(out, " :permissive ");
    out_bool(out, node.permissive);
    w!(out, " :roles ");
    out_list(out, &node.roles)?;
    w!(out, " :qual ");
    out_opt_node(out, node.qual)?;
    w!(out, " :with_check ");
    out_opt_node(out, node.with_check)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_alter_policy_stmt(out: &mut PgString<'_>, node: &AlterPolicyStmt<'_>) -> PgResult<()> {
    w!(out, "{{ALTERPOLICYSTMT");
    w!(out, " :policy_name ");
    out_str(out, node.policy_name);
    w!(out, " :table ");
    out_opt_range_var(out, node.table)?;
    w!(out, " :roles ");
    out_list(out, &node.roles)?;
    w!(out, " :qual ");
    out_opt_node(out, node.qual)?;
    w!(out, " :with_check ");
    out_opt_node(out, node.with_check)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_create_am_stmt(out: &mut PgString<'_>, node: &CreateAmStmt<'_>) -> PgResult<()> {
    w!(out, "{{CREATEAMSTMT");
    w!(out, " :amname ");
    out_str(out, node.amname);
    w!(out, " :handler_name ");
    out_list(out, &node.handler_name)?;
    w!(out, " :amtype ");
    out_char(out, node.amtype as u8)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_create_trig_stmt(out: &mut PgString<'_>, node: &CreateTrigStmt<'_>) -> PgResult<()> {
    w!(out, "{{CREATETRIGSTMT");
    w!(out, " :replace ");
    out_bool(out, node.replace);
    w!(out, " :isconstraint ");
    out_bool(out, node.isconstraint);
    w!(out, " :trigname ");
    out_str(out, node.trigname);
    w!(out, " :relation ");
    out_opt_range_var(out, node.relation)?;
    w!(out, " :funcname ");
    out_list(out, &node.funcname)?;
    w!(out, " :args ");
    out_list(out, &node.args)?;
    w!(out, " :row ");
    out_bool(out, node.row);
    w!(out, " :timing {}", node.timing);
    w!(out, " :events {}", node.events);
    w!(out, " :columns ");
    out_list(out, &node.columns)?;
    w!(out, " :whenClause ");
    out_opt_node(out, node.whenClause)?;
    w!(out, " :transitionRels ");
    out_list(out, &node.transitionRels)?;
    w!(out, " :deferrable ");
    out_bool(out, node.deferrable);
    w!(out, " :initdeferred ");
    out_bool(out, node.initdeferred);
    w!(out, " :constrrel ");
    out_opt_range_var(out, node.constrrel)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_create_event_trig_stmt(out: &mut PgString<'_>, node: &CreateEventTrigStmt<'_>) -> PgResult<()> {
    w!(out, "{{CREATEEVENTTRIGSTMT");
    w!(out, " :trigname ");
    out_str(out, node.trigname);
    w!(out, " :eventname ");
    out_str(out, node.eventname);
    w!(out, " :whenclause ");
    out_list(out, &node.whenclause)?;
    w!(out, " :funcname ");
    out_list(out, &node.funcname)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_alter_event_trig_stmt(out: &mut PgString<'_>, node: &AlterEventTrigStmt<'_>) -> PgResult<()> {
    w!(out, "{{ALTEREVENTTRIGSTMT");
    w!(out, " :trigname ");
    out_str(out, node.trigname);
    w!(out, " :tgenabled ");
    out_char(out, node.tgenabled as u8)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_create_p_lang_stmt(out: &mut PgString<'_>, node: &CreatePLangStmt<'_>) -> PgResult<()> {
    w!(out, "{{CREATEPLANGSTMT");
    w!(out, " :replace ");
    out_bool(out, node.replace);
    w!(out, " :plname ");
    out_str(out, node.plname);
    w!(out, " :plhandler ");
    out_list(out, &node.plhandler)?;
    w!(out, " :plinline ");
    out_list(out, &node.plinline)?;
    w!(out, " :plvalidator ");
    out_list(out, &node.plvalidator)?;
    w!(out, " :pltrusted ");
    out_bool(out, node.pltrusted);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_create_role_stmt(out: &mut PgString<'_>, node: &CreateRoleStmt<'_>) -> PgResult<()> {
    w!(out, "{{CREATEROLESTMT");
    w!(out, " :stmt_type {}", node.stmt_type as i32);
    w!(out, " :role ");
    out_str(out, node.role);
    w!(out, " :options ");
    out_list(out, &node.options)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_alter_role_stmt(out: &mut PgString<'_>, node: &AlterRoleStmt<'_>) -> PgResult<()> {
    w!(out, "{{ALTERROLESTMT");
    w!(out, " :role ");
    out_role_spec(out, node.role)?;
    w!(out, " :options ");
    out_list(out, &node.options)?;
    w!(out, " :action {}", node.action);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_alter_role_set_stmt(out: &mut PgString<'_>, node: &AlterRoleSetStmt<'_>) -> PgResult<()> {
    w!(out, "{{ALTERROLESETSTMT");
    w!(out, " :role ");
    match node.role { Some(r) => out_role_spec(out, r)?, None => w!(out, "<>") };
    w!(out, " :database ");
    out_str(out, node.database);
    w!(out, " :setstmt ");
    out_variable_set_stmt(out, node.setstmt)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_drop_role_stmt(out: &mut PgString<'_>, node: &DropRoleStmt<'_>) -> PgResult<()> {
    w!(out, "{{DROPROLESTMT");
    w!(out, " :roles ");
    out_list(out, &node.roles)?;
    w!(out, " :missing_ok ");
    out_bool(out, node.missing_ok);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_create_seq_stmt(out: &mut PgString<'_>, node: &CreateSeqStmt<'_>) -> PgResult<()> {
    w!(out, "{{CREATESEQSTMT");
    w!(out, " :sequence ");
    out_opt_range_var(out, node.sequence)?;
    w!(out, " :options ");
    out_list(out, &node.options)?;
    w!(out, " :ownerId {}", node.ownerId);
    w!(out, " :for_identity ");
    out_bool(out, node.for_identity);
    w!(out, " :if_not_exists ");
    out_bool(out, node.if_not_exists);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_alter_seq_stmt(out: &mut PgString<'_>, node: &AlterSeqStmt<'_>) -> PgResult<()> {
    w!(out, "{{ALTERSEQSTMT");
    w!(out, " :sequence ");
    out_opt_range_var(out, node.sequence)?;
    w!(out, " :options ");
    out_list(out, &node.options)?;
    w!(out, " :for_identity ");
    out_bool(out, node.for_identity);
    w!(out, " :missing_ok ");
    out_bool(out, node.missing_ok);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_define_stmt(out: &mut PgString<'_>, node: &DefineStmt<'_>) -> PgResult<()> {
    w!(out, "{{DEFINESTMT");
    w!(out, " :kind {}", node.kind as i32);
    w!(out, " :oldstyle ");
    out_bool(out, node.oldstyle);
    w!(out, " :defnames ");
    out_list(out, &node.defnames)?;
    w!(out, " :args ");
    out_list(out, &node.args)?;
    w!(out, " :definition ");
    out_list(out, &node.definition)?;
    w!(out, " :if_not_exists ");
    out_bool(out, node.if_not_exists);
    w!(out, " :replace ");
    out_bool(out, node.replace);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_create_domain_stmt(out: &mut PgString<'_>, node: &CreateDomainStmt<'_>) -> PgResult<()> {
    w!(out, "{{CREATEDOMAINSTMT");
    w!(out, " :domainname ");
    out_list(out, &node.domainname)?;
    w!(out, " :typeName ");
    out_opt_node(out, node.typeName)?;
    w!(out, " :collClause ");
    out_opt_node(out, node.collClause)?;
    w!(out, " :constraints ");
    out_list(out, &node.constraints)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_create_op_class_stmt(out: &mut PgString<'_>, node: &CreateOpClassStmt<'_>) -> PgResult<()> {
    w!(out, "{{CREATEOPCLASSSTMT");
    w!(out, " :opclassname ");
    out_list(out, &node.opclassname)?;
    w!(out, " :opfamilyname ");
    out_list(out, &node.opfamilyname)?;
    w!(out, " :amname ");
    out_str(out, node.amname);
    w!(out, " :datatype ");
    out_opt_node(out, node.datatype)?;
    w!(out, " :items ");
    out_list(out, &node.items)?;
    w!(out, " :isDefault ");
    out_bool(out, node.isDefault);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_create_op_class_item(out: &mut PgString<'_>, node: &CreateOpClassItem<'_>) -> PgResult<()> {
    w!(out, "{{CREATEOPCLASSITEM");
    w!(out, " :itemtype {}", node.itemtype);
    w!(out, " :name ");
    out_opt_node(out, node.name)?;
    w!(out, " :number {}", node.number);
    w!(out, " :order_family ");
    out_list(out, &node.order_family)?;
    w!(out, " :class_args ");
    out_list(out, &node.class_args)?;
    w!(out, " :storedtype ");
    out_opt_node(out, node.storedtype)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_create_op_family_stmt(out: &mut PgString<'_>, node: &CreateOpFamilyStmt<'_>) -> PgResult<()> {
    w!(out, "{{CREATEOPFAMILYSTMT");
    w!(out, " :opfamilyname ");
    out_list(out, &node.opfamilyname)?;
    w!(out, " :amname ");
    out_str(out, node.amname);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_alter_op_family_stmt(out: &mut PgString<'_>, node: &AlterOpFamilyStmt<'_>) -> PgResult<()> {
    w!(out, "{{ALTEROPFAMILYSTMT");
    w!(out, " :opfamilyname ");
    out_list(out, &node.opfamilyname)?;
    w!(out, " :amname ");
    out_str(out, node.amname);
    w!(out, " :isDrop ");
    out_bool(out, node.isDrop);
    w!(out, " :items ");
    out_list(out, &node.items)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_drop_stmt(out: &mut PgString<'_>, node: &DropStmt<'_>) -> PgResult<()> {
    w!(out, "{{DROPSTMT");
    w!(out, " :objects ");
    out_list(out, &node.objects)?;
    w!(out, " :removeType {}", node.removeType as i32);
    w!(out, " :behavior {}", node.behavior as i32);
    w!(out, " :missing_ok ");
    out_bool(out, node.missing_ok);
    w!(out, " :concurrent ");
    out_bool(out, node.concurrent);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_truncate_stmt(out: &mut PgString<'_>, node: &TruncateStmt<'_>) -> PgResult<()> {
    w!(out, "{{TRUNCATESTMT");
    w!(out, " :relations ");
    out_list(out, &node.relations)?;
    w!(out, " :restart_seqs ");
    out_bool(out, node.restart_seqs);
    w!(out, " :behavior {}", node.behavior as i32);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_comment_stmt(out: &mut PgString<'_>, node: &CommentStmt<'_>) -> PgResult<()> {
    w!(out, "{{COMMENTSTMT");
    w!(out, " :objtype {}", node.objtype as i32);
    w!(out, " :object ");
    out_opt_node(out, node.object)?;
    w!(out, " :comment ");
    out_str(out, node.comment);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_sec_label_stmt(out: &mut PgString<'_>, node: &SecLabelStmt<'_>) -> PgResult<()> {
    w!(out, "{{SECLABELSTMT");
    w!(out, " :objtype {}", node.objtype as i32);
    w!(out, " :object ");
    out_opt_node(out, node.object)?;
    w!(out, " :provider ");
    out_str(out, node.provider);
    w!(out, " :label ");
    out_str(out, node.label);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_declare_cursor_stmt(out: &mut PgString<'_>, node: &DeclareCursorStmt<'_>) -> PgResult<()> {
    w!(out, "{{DECLARECURSORSTMT");
    w!(out, " :portalname ");
    out_str(out, node.portalname);
    w!(out, " :options {}", node.options);
    w!(out, " :query ");
    out_opt_node(out, node.query)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_close_portal_stmt(out: &mut PgString<'_>, node: &ClosePortalStmt<'_>) -> PgResult<()> {
    w!(out, "{{CLOSEPORTALSTMT");
    w!(out, " :portalname ");
    out_str(out, node.portalname);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_fetch_stmt(out: &mut PgString<'_>, node: &FetchStmt<'_>) -> PgResult<()> {
    w!(out, "{{FETCHSTMT");
    w!(out, " :direction {}", node.direction as i32);
    w!(out, " :howMany {}", node.howMany);
    w!(out, " :portalname ");
    out_str(out, node.portalname);
    w!(out, " :ismove ");
    out_bool(out, node.ismove);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_index_stmt(out: &mut PgString<'_>, node: &IndexStmt<'_>) -> PgResult<()> {
    w!(out, "{{INDEXSTMT");
    w!(out, " :idxname ");
    out_str(out, node.idxname);
    w!(out, " :relation ");
    out_opt_range_var(out, node.relation)?;
    w!(out, " :accessMethod ");
    out_str(out, node.accessMethod);
    w!(out, " :tableSpace ");
    out_str(out, node.tableSpace);
    w!(out, " :indexParams ");
    out_list(out, &node.indexParams)?;
    w!(out, " :indexIncludingParams ");
    out_list(out, &node.indexIncludingParams)?;
    w!(out, " :options ");
    out_list(out, &node.options)?;
    w!(out, " :whereClause ");
    out_opt_node(out, node.whereClause)?;
    w!(out, " :excludeOpNames ");
    out_list(out, &node.excludeOpNames)?;
    w!(out, " :idxcomment ");
    out_str(out, node.idxcomment);
    w!(out, " :indexOid {}", node.indexOid);
    w!(out, " :oldNumber {}", node.oldNumber);
    w!(out, " :oldCreateSubid {}", node.oldCreateSubid);
    w!(out, " :oldFirstRelfilelocatorSubid {}", node.oldFirstRelfilelocatorSubid);
    w!(out, " :unique ");
    out_bool(out, node.unique);
    w!(out, " :nulls_not_distinct ");
    out_bool(out, node.nulls_not_distinct);
    w!(out, " :primary ");
    out_bool(out, node.primary);
    w!(out, " :isconstraint ");
    out_bool(out, node.isconstraint);
    w!(out, " :iswithoutoverlaps ");
    out_bool(out, node.iswithoutoverlaps);
    w!(out, " :deferrable ");
    out_bool(out, node.deferrable);
    w!(out, " :initdeferred ");
    out_bool(out, node.initdeferred);
    w!(out, " :transformed ");
    out_bool(out, node.transformed);
    w!(out, " :concurrent ");
    out_bool(out, node.concurrent);
    w!(out, " :if_not_exists ");
    out_bool(out, node.if_not_exists);
    w!(out, " :reset_default_tblspc ");
    out_bool(out, node.reset_default_tblspc);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_create_stats_stmt(out: &mut PgString<'_>, node: &CreateStatsStmt<'_>) -> PgResult<()> {
    w!(out, "{{CREATESTATSSTMT");
    w!(out, " :defnames ");
    out_list(out, &node.defnames)?;
    w!(out, " :stat_types ");
    out_list(out, &node.stat_types)?;
    w!(out, " :exprs ");
    out_list(out, &node.exprs)?;
    w!(out, " :relations ");
    out_list(out, &node.relations)?;
    w!(out, " :stxcomment ");
    out_str(out, node.stxcomment);
    w!(out, " :transformed ");
    out_bool(out, node.transformed);
    w!(out, " :if_not_exists ");
    out_bool(out, node.if_not_exists);
    w!(out, " :owner {}", node.owner);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_stats_elem(out: &mut PgString<'_>, node: &StatsElem<'_>) -> PgResult<()> {
    w!(out, "{{STATSELEM");
    w!(out, " :name ");
    out_str(out, node.name);
    w!(out, " :expr ");
    out_opt_node(out, node.expr)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_alter_stats_stmt(out: &mut PgString<'_>, node: &AlterStatsStmt<'_>) -> PgResult<()> {
    w!(out, "{{ALTERSTATSSTMT");
    w!(out, " :defnames ");
    out_list(out, &node.defnames)?;
    w!(out, " :stxstattarget ");
    out_opt_node(out, node.stxstattarget)?;
    w!(out, " :missing_ok ");
    out_bool(out, node.missing_ok);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_create_function_stmt(out: &mut PgString<'_>, node: &CreateFunctionStmt<'_>) -> PgResult<()> {
    w!(out, "{{CREATEFUNCTIONSTMT");
    w!(out, " :is_procedure ");
    out_bool(out, node.is_procedure);
    w!(out, " :replace ");
    out_bool(out, node.replace);
    w!(out, " :funcname ");
    out_list(out, &node.funcname)?;
    w!(out, " :parameters ");
    out_list(out, &node.parameters)?;
    w!(out, " :returnType ");
    out_opt_node(out, node.returnType)?;
    w!(out, " :options ");
    out_list(out, &node.options)?;
    w!(out, " :sql_body ");
    out_opt_node(out, node.sql_body)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_alter_function_stmt(out: &mut PgString<'_>, node: &AlterFunctionStmt<'_>) -> PgResult<()> {
    w!(out, "{{ALTERFUNCTIONSTMT");
    w!(out, " :objtype {}", node.objtype as i32);
    w!(out, " :func ");
    match node.func { Some(o) => out_object_with_args(out, o)?, None => w!(out, "<>") };
    w!(out, " :actions ");
    out_list(out, &node.actions)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_do_stmt(out: &mut PgString<'_>, node: &DoStmt<'_>) -> PgResult<()> {
    w!(out, "{{DOSTMT");
    w!(out, " :args ");
    out_list(out, &node.args)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_call_stmt(out: &mut PgString<'_>, node: &CallStmt<'_>) -> PgResult<()> {
    w!(out, "{{CALLSTMT");
    w!(out, " :funccall ");
    match node.funccall { Some(f) => out_func_call(out, f)?, None => w!(out, "<>") };
    w!(out, " :funcexpr ");
    match node.funcexpr { Some(f) => out_func_expr(out, f)?, None => w!(out, "<>") };
    w!(out, " :outargs ");
    out_list(out, &node.outargs)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_rename_stmt(out: &mut PgString<'_>, node: &RenameStmt<'_>) -> PgResult<()> {
    w!(out, "{{RENAMESTMT");
    w!(out, " :renameType {}", node.renameType as i32);
    w!(out, " :relationType {}", node.relationType as i32);
    w!(out, " :relation ");
    out_opt_range_var(out, node.relation)?;
    w!(out, " :object ");
    out_opt_node(out, node.object)?;
    w!(out, " :subname ");
    out_str(out, node.subname);
    w!(out, " :newname ");
    out_str(out, node.newname);
    w!(out, " :behavior {}", node.behavior as i32);
    w!(out, " :missing_ok ");
    out_bool(out, node.missing_ok);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_alter_object_schema_stmt(out: &mut PgString<'_>, node: &AlterObjectSchemaStmt<'_>) -> PgResult<()> {
    w!(out, "{{ALTEROBJECTSCHEMASTMT");
    w!(out, " :objectType {}", node.objectType as i32);
    w!(out, " :relation ");
    out_opt_range_var(out, node.relation)?;
    w!(out, " :object ");
    out_opt_node(out, node.object)?;
    w!(out, " :newschema ");
    out_str(out, node.newschema);
    w!(out, " :missing_ok ");
    out_bool(out, node.missing_ok);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_alter_owner_stmt(out: &mut PgString<'_>, node: &AlterOwnerStmt<'_>) -> PgResult<()> {
    w!(out, "{{ALTEROWNERSTMT");
    w!(out, " :objectType {}", node.objectType as i32);
    w!(out, " :relation ");
    out_opt_range_var(out, node.relation)?;
    w!(out, " :object ");
    out_opt_node(out, node.object)?;
    w!(out, " :newowner ");
    match node.newowner { Some(r) => out_role_spec(out, r)?, None => w!(out, "<>") };
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_alter_operator_stmt(out: &mut PgString<'_>, node: &AlterOperatorStmt<'_>) -> PgResult<()> {
    w!(out, "{{ALTEROPERATORSTMT");
    w!(out, " :opername ");
    out_opt_node(out, node.opername)?;
    w!(out, " :options ");
    out_list(out, &node.options)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_alter_type_stmt(out: &mut PgString<'_>, node: &AlterTypeStmt<'_>) -> PgResult<()> {
    w!(out, "{{ALTERTYPESTMT");
    w!(out, " :typeName ");
    out_list(out, &node.typeName)?;
    w!(out, " :options ");
    out_list(out, &node.options)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_rule_stmt(out: &mut PgString<'_>, node: &RuleStmt<'_>) -> PgResult<()> {
    w!(out, "{{RULESTMT");
    w!(out, " :relation ");
    out_opt_range_var(out, node.relation)?;
    w!(out, " :rulename ");
    out_token(out, node.rulename);
    w!(out, " :whereClause ");
    out_opt_node(out, node.whereClause)?;
    w!(out, " :event {}", node.event as i32);
    w!(out, " :instead ");
    out_bool(out, node.instead);
    w!(out, " :actions ");
    out_list(out, &node.actions)?;
    w!(out, " :replace ");
    out_bool(out, node.replace);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_listen_stmt(out: &mut PgString<'_>, node: &ListenStmt<'_>) -> PgResult<()> {
    w!(out, "{{LISTENSTMT");
    w!(out, " :conditionname ");
    out_str(out, node.conditionname);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_unlisten_stmt(out: &mut PgString<'_>, node: &UnlistenStmt<'_>) -> PgResult<()> {
    w!(out, "{{UNLISTENSTMT");
    w!(out, " :conditionname ");
    out_str(out, node.conditionname);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_composite_type_stmt(out: &mut PgString<'_>, node: &CompositeTypeStmt<'_>) -> PgResult<()> {
    w!(out, "{{COMPOSITETYPESTMT");
    w!(out, " :typevar ");
    out_opt_range_var(out, node.typevar)?;
    w!(out, " :coldeflist ");
    out_list(out, &node.coldeflist)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_create_enum_stmt(out: &mut PgString<'_>, node: &CreateEnumStmt<'_>) -> PgResult<()> {
    w!(out, "{{CREATEENUMSTMT");
    w!(out, " :typeName ");
    out_list(out, &node.typeName)?;
    w!(out, " :vals ");
    out_list(out, &node.vals)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_create_range_stmt(out: &mut PgString<'_>, node: &CreateRangeStmt<'_>) -> PgResult<()> {
    w!(out, "{{CREATERANGESTMT");
    w!(out, " :typeName ");
    out_list(out, &node.typeName)?;
    w!(out, " :params ");
    out_list(out, &node.params)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_alter_enum_stmt(out: &mut PgString<'_>, node: &AlterEnumStmt<'_>) -> PgResult<()> {
    w!(out, "{{ALTERENUMSTMT");
    w!(out, " :typeName ");
    out_list(out, &node.typeName)?;
    w!(out, " :oldVal ");
    out_str(out, node.oldVal);
    w!(out, " :newVal ");
    out_str(out, node.newVal);
    w!(out, " :newValNeighbor ");
    out_str(out, node.newValNeighbor);
    w!(out, " :newValIsAfter ");
    out_bool(out, node.newValIsAfter);
    w!(out, " :skipIfNewValExists ");
    out_bool(out, node.skipIfNewValExists);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_view_stmt(out: &mut PgString<'_>, node: &ViewStmt<'_>) -> PgResult<()> {
    w!(out, "{{VIEWSTMT");
    w!(out, " :view ");
    out_opt_range_var(out, node.view)?;
    w!(out, " :aliases ");
    out_list(out, &node.aliases)?;
    w!(out, " :query ");
    out_opt_node(out, node.query)?;
    w!(out, " :replace ");
    out_bool(out, node.replace);
    w!(out, " :options ");
    out_list(out, &node.options)?;
    w!(out, " :withCheckOption {}", node.withCheckOption as i32);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_load_stmt(out: &mut PgString<'_>, node: &LoadStmt<'_>) -> PgResult<()> {
    w!(out, "{{LOADSTMT");
    w!(out, " :filename ");
    out_token(out, node.filename);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_createdb_stmt(out: &mut PgString<'_>, node: &CreatedbStmt<'_>) -> PgResult<()> {
    w!(out, "{{CREATEDBSTMT");
    w!(out, " :dbname ");
    out_str(out, node.dbname);
    w!(out, " :options ");
    out_list(out, &node.options)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_alter_database_stmt(out: &mut PgString<'_>, node: &AlterDatabaseStmt<'_>) -> PgResult<()> {
    w!(out, "{{ALTERDATABASESTMT");
    w!(out, " :dbname ");
    out_str(out, node.dbname);
    w!(out, " :options ");
    out_list(out, &node.options)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_alter_database_refresh_coll_stmt(out: &mut PgString<'_>, node: &AlterDatabaseRefreshCollStmt<'_>) -> PgResult<()> {
    w!(out, "{{ALTERDATABASEREFRESHCOLLSTMT");
    w!(out, " :dbname ");
    out_str(out, node.dbname);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_alter_database_set_stmt(out: &mut PgString<'_>, node: &AlterDatabaseSetStmt<'_>) -> PgResult<()> {
    w!(out, "{{ALTERDATABASESETSTMT");
    w!(out, " :dbname ");
    out_str(out, node.dbname);
    w!(out, " :setstmt ");
    out_opt_node(out, node.setstmt)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_dropdb_stmt(out: &mut PgString<'_>, node: &DropdbStmt<'_>) -> PgResult<()> {
    w!(out, "{{DROPDBSTMT");
    w!(out, " :dbname ");
    out_str(out, node.dbname);
    w!(out, " :missing_ok ");
    out_bool(out, node.missing_ok);
    w!(out, " :options ");
    out_list(out, &node.options)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_alter_system_stmt(out: &mut PgString<'_>, node: &AlterSystemStmt<'_>) -> PgResult<()> {
    w!(out, "{{ALTERSYSTEMSTMT");
    w!(out, " :setstmt ");
    out_variable_set_stmt(out, node.setstmt)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_cluster_stmt(out: &mut PgString<'_>, node: &ClusterStmt<'_>) -> PgResult<()> {
    w!(out, "{{CLUSTERSTMT");
    w!(out, " :relation ");
    out_opt_node(out, node.relation)?;
    w!(out, " :indexname ");
    out_str(out, node.indexname);
    w!(out, " :params ");
    out_list(out, &node.params)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_vacuum_stmt(out: &mut PgString<'_>, node: &VacuumStmt<'_>) -> PgResult<()> {
    w!(out, "{{VACUUMSTMT");
    w!(out, " :options ");
    out_list(out, &node.options)?;
    w!(out, " :rels ");
    out_list(out, &node.rels)?;
    w!(out, " :is_vacuumcmd ");
    out_bool(out, node.is_vacuumcmd);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_vacuum_relation(out: &mut PgString<'_>, node: &VacuumRelation<'_>) -> PgResult<()> {
    w!(out, "{{VACUUMRELATION");
    w!(out, " :relation ");
    out_opt_node(out, node.relation)?;
    w!(out, " :oid {}", node.oid);
    w!(out, " :va_cols ");
    out_list(out, &node.va_cols)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_explain_stmt(out: &mut PgString<'_>, node: &ExplainStmt<'_>) -> PgResult<()> {
    w!(out, "{{EXPLAINSTMT");
    w!(out, " :query ");
    out_opt_node(out, node.query)?;
    w!(out, " :options ");
    out_list(out, &node.options)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_create_table_as_stmt(out: &mut PgString<'_>, node: &CreateTableAsStmt<'_>) -> PgResult<()> {
    w!(out, "{{CREATETABLEASSTMT");
    w!(out, " :query ");
    out_opt_node(out, node.query)?;
    w!(out, " :into ");
    out_opt_node(out, node.into)?;
    w!(out, " :objtype {}", node.objtype as i32);
    w!(out, " :is_select_into ");
    out_bool(out, node.is_select_into);
    w!(out, " :if_not_exists ");
    out_bool(out, node.if_not_exists);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_refresh_mat_view_stmt(out: &mut PgString<'_>, node: &RefreshMatViewStmt<'_>) -> PgResult<()> {
    w!(out, "{{REFRESHMATVIEWSTMT");
    w!(out, " :concurrent ");
    out_bool(out, node.concurrent);
    w!(out, " :skipData ");
    out_bool(out, node.skipData);
    w!(out, " :relation ");
    out_opt_range_var(out, node.relation)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_check_point_stmt(out: &mut PgString<'_>, _node: &CheckPointStmt) -> PgResult<()> {
    w!(out, "{{CHECKPOINTSTMT");
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_discard_stmt(out: &mut PgString<'_>, node: &DiscardStmt) -> PgResult<()> {
    w!(out, "{{DISCARDSTMT");
    w!(out, " :target {}", node.target as i32);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_lock_stmt(out: &mut PgString<'_>, node: &LockStmt<'_>) -> PgResult<()> {
    w!(out, "{{LOCKSTMT");
    w!(out, " :relations ");
    out_list(out, &node.relations)?;
    w!(out, " :mode {}", node.mode);
    w!(out, " :nowait ");
    out_bool(out, node.nowait);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_constraints_set_stmt(out: &mut PgString<'_>, node: &ConstraintsSetStmt<'_>) -> PgResult<()> {
    w!(out, "{{CONSTRAINTSSETSTMT");
    w!(out, " :constraints ");
    out_list(out, &node.constraints)?;
    w!(out, " :deferred ");
    out_bool(out, node.deferred);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_reindex_stmt(out: &mut PgString<'_>, node: &ReindexStmt<'_>) -> PgResult<()> {
    w!(out, "{{REINDEXSTMT");
    w!(out, " :kind {}", node.kind as i32);
    w!(out, " :relation ");
    out_opt_node(out, node.relation)?;
    w!(out, " :name ");
    out_str(out, node.name);
    w!(out, " :params ");
    out_list(out, &node.params)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_create_conversion_stmt(out: &mut PgString<'_>, node: &CreateConversionStmt<'_>) -> PgResult<()> {
    w!(out, "{{CREATECONVERSIONSTMT");
    w!(out, " :conversion_name ");
    out_list(out, &node.conversion_name)?;
    w!(out, " :for_encoding_name ");
    out_str(out, node.for_encoding_name);
    w!(out, " :to_encoding_name ");
    out_str(out, node.to_encoding_name);
    w!(out, " :func_name ");
    out_list(out, &node.func_name)?;
    w!(out, " :def ");
    out_bool(out, node.def);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_create_cast_stmt(out: &mut PgString<'_>, node: &CreateCastStmt<'_>) -> PgResult<()> {
    w!(out, "{{CREATECASTSTMT");
    w!(out, " :sourcetype ");
    out_opt_node(out, node.sourcetype)?;
    w!(out, " :targettype ");
    out_opt_node(out, node.targettype)?;
    w!(out, " :func ");
    out_opt_node(out, node.func)?;
    w!(out, " :context {}", node.context as i32);
    w!(out, " :inout ");
    out_bool(out, node.inout);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_create_transform_stmt(out: &mut PgString<'_>, node: &CreateTransformStmt<'_>) -> PgResult<()> {
    w!(out, "{{CREATETRANSFORMSTMT");
    w!(out, " :replace ");
    out_bool(out, node.replace);
    w!(out, " :type_name ");
    out_opt_node(out, node.type_name)?;
    w!(out, " :lang ");
    out_str(out, node.lang);
    w!(out, " :fromsql ");
    out_opt_node(out, node.fromsql)?;
    w!(out, " :tosql ");
    out_opt_node(out, node.tosql)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_drop_owned_stmt(out: &mut PgString<'_>, node: &DropOwnedStmt<'_>) -> PgResult<()> {
    w!(out, "{{DROPOWNEDSTMT");
    w!(out, " :roles ");
    out_list(out, &node.roles)?;
    w!(out, " :behavior {}", node.behavior as i32);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_reassign_owned_stmt(out: &mut PgString<'_>, node: &ReassignOwnedStmt<'_>) -> PgResult<()> {
    w!(out, "{{REASSIGNOWNEDSTMT");
    w!(out, " :roles ");
    out_list(out, &node.roles)?;
    w!(out, " :newrole ");
    out_role_spec(out, node.newrole)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_alter_ts_dictionary_stmt(out: &mut PgString<'_>, node: &AlterTSDictionaryStmt<'_>) -> PgResult<()> {
    w!(out, "{{ALTERTSDICTIONARYSTMT");
    w!(out, " :dictname ");
    out_list(out, &node.dictname)?;
    w!(out, " :options ");
    out_list(out, &node.options)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_alter_ts_configuration_stmt(out: &mut PgString<'_>, node: &AlterTSConfigurationStmt<'_>) -> PgResult<()> {
    w!(out, "{{ALTERTSCONFIGURATIONSTMT");
    w!(out, " :kind {}", node.kind as i32);
    w!(out, " :cfgname ");
    out_list(out, &node.cfgname)?;
    w!(out, " :tokentype ");
    out_list(out, &node.tokentype)?;
    w!(out, " :dicts ");
    out_list(out, &node.dicts)?;
    w!(out, " :override ");
    out_bool(out, node.r#override);
    w!(out, " :replace ");
    out_bool(out, node.replace);
    w!(out, " :missing_ok ");
    out_bool(out, node.missing_ok);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_publication_table(out: &mut PgString<'_>, node: &PublicationTable<'_>) -> PgResult<()> {
    w!(out, "{{PUBLICATIONTABLE");
    w!(out, " :relation ");
    out_opt_range_var(out, node.relation)?;
    w!(out, " :whereClause ");
    out_opt_node(out, node.whereClause)?;
    w!(out, " :columns ");
    out_list(out, &node.columns)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_publication_obj_spec(out: &mut PgString<'_>, node: &PublicationObjSpec<'_>) -> PgResult<()> {
    w!(out, "{{PUBLICATIONOBJSPEC");
    w!(out, " :pubobjtype {}", node.pubobjtype as i32);
    w!(out, " :name ");
    out_str(out, node.name);
    w!(out, " :pubtable ");
    match node.pubtable { Some(p) => out_publication_table(out, p)?, None => w!(out, "<>") };
    w!(out, " :location {}", loc(node.location));
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_create_publication_stmt(out: &mut PgString<'_>, node: &CreatePublicationStmt<'_>) -> PgResult<()> {
    w!(out, "{{CREATEPUBLICATIONSTMT");
    w!(out, " :pubname ");
    out_str(out, node.pubname);
    w!(out, " :options ");
    out_list(out, &node.options)?;
    w!(out, " :pubobjects ");
    out_list(out, &node.pubobjects)?;
    w!(out, " :for_all_tables ");
    out_bool(out, node.for_all_tables);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_alter_publication_stmt(out: &mut PgString<'_>, node: &AlterPublicationStmt<'_>) -> PgResult<()> {
    w!(out, "{{ALTERPUBLICATIONSTMT");
    w!(out, " :pubname ");
    out_str(out, node.pubname);
    w!(out, " :options ");
    out_list(out, &node.options)?;
    w!(out, " :pubobjects ");
    out_list(out, &node.pubobjects)?;
    w!(out, " :for_all_tables ");
    out_bool(out, node.for_all_tables);
    w!(out, " :action {}", node.action as i32);
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_create_subscription_stmt(out: &mut PgString<'_>, node: &CreateSubscriptionStmt<'_>) -> PgResult<()> {
    w!(out, "{{CREATESUBSCRIPTIONSTMT");
    w!(out, " :subname ");
    out_str(out, node.subname);
    w!(out, " :conninfo ");
    out_str(out, node.conninfo);
    w!(out, " :publication ");
    out_list(out, &node.publication)?;
    w!(out, " :options ");
    out_list(out, &node.options)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_alter_subscription_stmt(out: &mut PgString<'_>, node: &AlterSubscriptionStmt<'_>) -> PgResult<()> {
    w!(out, "{{ALTERSUBSCRIPTIONSTMT");
    w!(out, " :kind {}", node.kind as i32);
    w!(out, " :subname ");
    out_str(out, node.subname);
    w!(out, " :conninfo ");
    out_str(out, node.conninfo);
    w!(out, " :publication ");
    out_list(out, &node.publication)?;
    w!(out, " :options ");
    out_list(out, &node.options)?;
    w!(out, "}}");
    Ok(())
}

pub(super) fn out_drop_subscription_stmt(out: &mut PgString<'_>, node: &DropSubscriptionStmt<'_>) -> PgResult<()> {
    w!(out, "{{DROPSUBSCRIPTIONSTMT");
    w!(out, " :subname ");
    out_str(out, node.subname);
    w!(out, " :missing_ok ");
    out_bool(out, node.missing_ok);
    w!(out, " :behavior {}", node.behavior as i32);
    w!(out, "}}");
    Ok(())
}
